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

use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::process::Stdio;
use std::sync::Arc;

use chrono::Utc;
use tokio::process::Command;

use crate::provider::{SandboxError, SandboxProvider, Tunnel};
use crate::{BoxHandle, BoxStatus, PowerState, SandboxSpec};

/// Boxed async result returned by [`CommandRunner`].
pub type CommandFuture<'a, T> = Pin<Box<dyn Future<Output = Result<T, SandboxError>> + Send + 'a>>;

/// Runs one external command (`terraform`/`gcloud`) to completion.
/// Injectable so `GcpProvider`'s lifecycle sequencing — ensure's
/// init→apply→output, connect's wake-then-forward — is unit-tested against
/// scripted output without a real GCP project (see this module's tests).
pub trait CommandRunner: Send + Sync + std::fmt::Debug {
    /// Run `program` with `args` and the extra `env` overlaid on the
    /// inherited environment, resolving to captured stdout on a zero exit or
    /// a [`SandboxError`] carrying the program name and stderr otherwise.
    /// `env` is the provider's explicitly-injected credentials (#1047), never
    /// ambient state. Spawn failures surface as [`SandboxError::Spawn`].
    fn run<'a>(
        &'a self,
        program: &'a str,
        args: &'a [String],
        env: &'a [(String, String)],
    ) -> CommandFuture<'a, String>;
}

/// The production runner: shells out with stdin closed and captures output.
#[derive(Debug, Clone)]
pub struct SystemRunner;

impl CommandRunner for SystemRunner {
    fn run<'a>(
        &'a self,
        program: &'a str,
        args: &'a [String],
        env: &'a [(String, String)],
    ) -> CommandFuture<'a, String> {
        Box::pin(async move {
            let output = Command::new(program)
                .args(args)
                .envs(env.iter().map(|(k, v)| (k, v)))
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
        })
    }
}

/// How the GCP provider authenticates, threaded explicitly into every
/// `gcloud`/`terraform` call so the box lifecycle never depends on ambient
/// `gcloud auth login`/ADC and never mutates the user's own gcloud config
/// (#1047). All fields optional; nothing set → [ambient](Self::is_ambient)
/// credentials, the legacy behavior.
#[derive(Debug, Clone, Default)]
pub struct GcpAuth {
    /// A service-account key (or any `GOOGLE_APPLICATION_CREDENTIALS`
    /// -compatible credential file). Used for gcloud (via
    /// `CLOUDSDK_AUTH_CREDENTIAL_FILE_OVERRIDE`) and for the terraform
    /// `google` provider (via `GOOGLE_APPLICATION_CREDENTIALS`) alike.
    pub service_account_key: Option<PathBuf>,
    /// Service account to impersonate; the base credentials mint tokens for
    /// it (gcloud + terraform both honor the matching env).
    pub impersonate_service_account: Option<String>,
    /// Provider-scoped `CLOUDSDK_CONFIG` dir, honored only alongside a
    /// `service_account_key` (the base credential it isolates); scoping
    /// without a base would strand impersonation, so it is ignored there.
    /// `None` leaves gcloud on its default config (ambient path).
    pub config_dir: Option<PathBuf>,
}

impl GcpAuth {
    /// True when no credentials are configured — the provider falls back to
    /// whatever ambient auth the machine has, exactly as before #1047. In
    /// this mode [`command_env`](Self::command_env) injects nothing, so
    /// today's behavior is preserved byte-for-byte.
    pub fn is_ambient(&self) -> bool {
        self.service_account_key.is_none() && self.impersonate_service_account.is_none()
    }

    /// The environment overlaid on every `gcloud`/`terraform` invocation.
    /// Empty in the ambient path; otherwise the credential envs both tools
    /// read, so auth is explicit rather than inherited from the user's shell.
    pub fn command_env(&self) -> Vec<(String, String)> {
        if self.is_ambient() {
            return Vec::new();
        }
        let mut env = Vec::new();
        if let Some(key) = &self.service_account_key {
            let path = key.display().to_string();
            // Scope gcloud's config *only* when we supply our own base
            // credential (this key). Scoping to an empty dir with no base —
            // e.g. impersonation with no key — would cut off the ambient /
            // metadata credentials the token exchange needs, so the
            // impersonation could never resolve a base. With a key present,
            // the scoped dir isolates lazybox's gcloud state from the user's
            // own `~/.config/gcloud`.
            if let Some(dir) = &self.config_dir {
                env.push(("CLOUDSDK_CONFIG".to_string(), dir.display().to_string()));
            }
            // gcloud reads its own credential store, not GOOGLE_APPLICATION_
            // CREDENTIALS; the override points it at the key statelessly (no
            // `activate-service-account` write, so concurrent CLI + r-spawn
            // processes never race the scoped credential db).
            env.push((
                "CLOUDSDK_AUTH_CREDENTIAL_FILE_OVERRIDE".to_string(),
                path.clone(),
            ));
            // The terraform `google` provider reads this one.
            env.push(("GOOGLE_APPLICATION_CREDENTIALS".to_string(), path));
        }
        if let Some(sa) = &self.impersonate_service_account {
            env.push((
                "CLOUDSDK_AUTH_IMPERSONATE_SERVICE_ACCOUNT".to_string(),
                sa.clone(),
            ));
            env.push(("GOOGLE_IMPERSONATE_SERVICE_ACCOUNT".to_string(), sa.clone()));
        }
        env
    }
}

/// Keepalive cadence for the IAP forward — matches `connect.sh` and the
/// in-process supervisor so all three damp on the same schedule.
const SERVER_ALIVE_INTERVAL: u64 = 30;
const SERVER_ALIVE_COUNT_MAX: u64 = 3;
/// Seconds the reachability probe's SSH waits before declaring the box
/// unreachable — short so `status` stays snappy.
const PROBE_CONNECT_TIMEOUT: u64 = 8;

/// Driver for boxes on Google Compute Engine.
#[derive(Debug, Clone)]
pub struct GcpProvider {
    /// The `terraform/sandbox/gcp` module directory `ensure`/`destroy`
    /// runs against. A project override (obin) points this at its own
    /// module. The module source is read-only and shared across boxes;
    /// per-box state lives in [`state_file`], not here.
    ///
    /// [`state_file`]: Self::state_file
    pub terraform_dir: PathBuf,
    /// The Terraform state file for *this* box, isolated per worktree and
    /// kept out of the module source tree. Without this every worktree
    /// would share the module dir's default `terraform.tfstate`, so a
    /// second box's `apply` — same resource addresses, different
    /// `instance_name` — would replace the first box in place.
    pub state_file: PathBuf,
    /// SSH/gcloud user for the IAP connect; `None` uses gcloud's default.
    pub user: Option<String>,
    /// Absolute daemon-socket path on the box that `connect` forwards.
    pub remote_socket: String,
    /// Local socket the forward binds — the path `--connect` dials.
    pub local_socket: PathBuf,
    /// Runs `terraform`/`gcloud`; `Arc::new(SystemRunner)` in production, a
    /// scripted fake under test. Named like every other field so the struct
    /// literal stays the construction path — no positional constructor to
    /// transpose the two `PathBuf`s through.
    pub runner: Arc<dyn CommandRunner>,
    /// Credentials injected into every `gcloud`/`terraform` call, so the
    /// lifecycle authenticates off configured creds rather than ambient
    /// `gcloud auth login`/ADC (#1047).
    pub auth: GcpAuth,
}

impl GcpProvider {
    /// `terraform -chdir=<dir> init -input=false` — prepare the module's
    /// providers/backend. Run before the first `apply`; a fresh module dir
    /// has no `.terraform/`, so `apply` alone would hard-fail with "please
    /// run terraform init". Idempotent, so it is cheap to run every time.
    fn init_command(&self) -> (String, Vec<String>) {
        (
            "terraform".to_string(),
            vec![
                format!("-chdir={}", self.terraform_dir.display()),
                "init".to_string(),
                "-input=false".to_string(),
            ],
        )
    }

    /// The `terraform -chdir=<dir> <action> …` invocation for `ensure`
    /// (`apply`) / `destroy`. `apply` passes the full deployment vars;
    /// `destroy` passes only the identity vars recovered from the handle
    /// (the module's other variables carry defaults, so state teardown
    /// needs no deployment recipe). `-state` pins this box's isolated
    /// state file so two worktrees driving the same module dir never share
    /// state and clobber each other.
    fn terraform_command(&self, action: &str, vars: &[String]) -> (String, Vec<String>) {
        let mut args = vec![
            format!("-chdir={}", self.terraform_dir.display()),
            action.to_string(),
            "-auto-approve".to_string(),
            "-input=false".to_string(),
            format!("-state={}", self.state_file.display()),
        ];
        for v in vars {
            args.push("-var".to_string());
            args.push(v.clone());
        }
        ("terraform".to_string(), args)
    }

    /// `terraform output -json` — read the applied module's outputs
    /// (instance name + zone) back so the handle addresses the real box.
    /// Reads the same isolated `-state` the matching `apply` wrote.
    fn output_command(&self) -> (String, Vec<String>) {
        (
            "terraform".to_string(),
            vec![
                format!("-chdir={}", self.terraform_dir.display()),
                "output".to_string(),
                "-json".to_string(),
                format!("-state={}", self.state_file.display()),
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

    /// A one-shot IAP SSH that runs `true` with a short connect timeout —
    /// the reachability probe. It succeeds only when SSH-over-IAP actually
    /// completes, which is what distinguishes a box that is `Running` but
    /// not yet reachable (the wake→sshd window) from one that is.
    fn reachable_probe_command(&self, handle: &BoxHandle) -> (String, Vec<String>) {
        let dest = match &self.user {
            Some(u) => format!("{u}@{}", handle.id),
            None => handle.id.clone(),
        };
        (
            "gcloud".to_string(),
            vec![
                "compute".to_string(),
                "ssh".to_string(),
                dest,
                "--quiet".to_string(),
                format!("--zone={}", handle.zone),
                format!("--project={}", handle.project),
                "--tunnel-through-iap".to_string(),
                "--command=true".to_string(),
                "--".to_string(),
                "-o".to_string(),
                "BatchMode=yes".to_string(),
                "-o".to_string(),
                "StrictHostKeyChecking=accept-new".to_string(),
                "-o".to_string(),
                format!("ConnectTimeout={PROBE_CONNECT_TIMEOUT}"),
            ],
        )
    }

    /// The IAP SSH that rebuilds the box's daemon at `sha` and restarts it
    /// — the recovery path for a wire-fingerprint mismatch (#977) that needs
    /// no reboot (changing GCE metadata does not re-run the startup script
    /// on a live box). An empty `sha` tells the helper to track the default
    /// branch tip.
    ///
    /// The build itself is a 10+ minute compile, so it must NOT run in the SSH
    /// session's own process tree: a dropped IAP link or a slept laptop would
    /// SIGHUP it mid-build (the very #903 hazard the boot path avoids). It is
    /// instead launched in a **supervised systemd transient unit** that keeps
    /// running to completion even if this SSH dies. `--wait --pipe` streams the
    /// build and propagates its exit status while connected; `EnvironmentFile`
    /// picks up the same `/etc/lazybox/build.env` the boot build reads (the `-`
    /// prefix makes it optional), so a box provisioned with non-default paths
    /// rebuilds against its real config rather than the helper's fallbacks.
    pub fn rebuild_command(&self, handle: &BoxHandle, sha: &str) -> (String, Vec<String>) {
        let dest = match &self.user {
            Some(u) => format!("{u}@{}", handle.id),
            None => handle.id.clone(),
        };
        let remote = format!(
            "sudo systemd-run --wait --pipe --collect --unit=lazybox-rebuild \
             --property=TimeoutStartSec=7200 \
             --property=EnvironmentFile=-/etc/lazybox/build.env \
             /usr/local/bin/lazybox-build.sh {sha}"
        );
        (
            "gcloud".to_string(),
            vec![
                "compute".to_string(),
                "ssh".to_string(),
                dest,
                "--quiet".to_string(),
                format!("--zone={}", handle.zone),
                format!("--project={}", handle.project),
                "--tunnel-through-iap".to_string(),
                format!("--command={remote}"),
                "--".to_string(),
                "-o".to_string(),
                "BatchMode=yes".to_string(),
                "-o".to_string(),
                "StrictHostKeyChecking=accept-new".to_string(),
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
            // The forward is spawned outside the `CommandRunner`, so carry the
            // credentials on it — the IAP SSH authenticates off the same
            // injected creds as every other op (#1047).
            env: self.auth.command_env(),
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

impl GcpProvider {
    /// Run a command through the injected runner, overlaying the provider's
    /// credentials env on every call — the single choke point that makes auth
    /// explicit rather than ambient (#1047).
    async fn run(&self, program: &str, args: &[String]) -> Result<String, SandboxError> {
        self.runner
            .run(program, args, &self.auth.command_env())
            .await
    }

    /// `gcloud auth print-access-token` — the preflight probe. It mints a
    /// token from whatever [`GcpAuth`] resolves to (the injected key, an
    /// impersonation target, or ambient creds), so it fails fast and locally
    /// when credentials are absent or unusable.
    fn auth_probe_command() -> (String, Vec<String>) {
        (
            "gcloud".to_string(),
            vec!["auth".to_string(), "print-access-token".to_string()],
        )
    }

    /// Read the box's power state (describe only — no reachability probe),
    /// shared by `status` and the `connect` wake decision so the latter
    /// doesn't pay for a reachability round-trip it doesn't need.
    async fn power(&self, handle: &BoxHandle) -> Result<PowerState, SandboxError> {
        let (prog, args) = Self::describe_command(handle);
        Ok(parse_power_state(&self.run(&prog, &args).await?))
    }
}

/// The actionable message a failed [`GcpProvider::check_auth`] carries — its
/// remedy depends on whether credentials were configured at all. Pure so both
/// branches are unit-tested.
fn auth_failure_message(auth: &GcpAuth, detail: &str) -> String {
    if auth.is_ambient() {
        format!(
            "gcp credentials not configured: set `sandbox.auth.service_account_key` (headless) \
             or run `gcloud auth login` — {detail}"
        )
    } else {
        format!("gcp credentials not usable (misconfigured or expired): {detail}")
    }
}

impl SandboxProvider for GcpProvider {
    fn id(&self) -> &str {
        "gcp"
    }

    async fn check_auth(&self) -> Result<(), SandboxError> {
        // A configured key is validated *offline*: open it (catching a wrong
        // path or an unreadable file — the common misconfiguration) rather
        // than mint a token. A service-account key is a long-lived credential
        // that doesn't expire, so a network probe on every op — including the
        // deliberately snappy `status` — would add a token round-trip that
        // catches almost nothing the first real op wouldn't. The scoped config
        // dir it isolates into must exist before gcloud writes there.
        if let Some(key) = &self.auth.service_account_key {
            std::fs::File::open(key).map_err(|e| {
                SandboxError::Config(format!(
                    "gcp credentials: service account key {} unreadable: {e} — point \
                     `sandbox.auth.service_account_key` at a readable key file",
                    key.display()
                ))
            })?;
            if let Some(dir) = &self.auth.config_dir {
                std::fs::create_dir_all(dir).map_err(|e| {
                    SandboxError::Config(format!("create gcloud config dir {}: {e}", dir.display()))
                })?;
            }
            return Ok(());
        }
        // No offline-checkable base credential (ambient, or impersonation whose
        // base is ambient/metadata): a token probe is the only way to tell a
        // configured-and-working setup from a missing login, and it yields the
        // actionable fix hint instead of raw gcloud/terraform stderr deep in
        // the first real op.
        let (prog, args) = Self::auth_probe_command();
        self.run(&prog, &args)
            .await
            .map_err(|e| SandboxError::Config(auth_failure_message(&self.auth, &e.to_string())))?;
        Ok(())
    }

    async fn ensure(&self, spec: &SandboxSpec) -> Result<BoxHandle, SandboxError> {
        // Init first: a fresh module dir has no providers, so a bare
        // `apply` would fail before touching any infrastructure.
        let (prog, args) = self.init_command();
        self.run(&prog, &args).await?;

        let (prog, args) = self.terraform_command("apply", &spec.tf_vars());
        self.run(&prog, &args).await?;

        let (prog, args) = self.output_command();
        let outputs = self.run(&prog, &args).await?;
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
        self.run(&prog, &args).await.map(|_| ())
    }

    async fn stop(&self, handle: &BoxHandle) -> Result<(), SandboxError> {
        let (prog, args) = Self::instance_command(handle, "stop");
        self.run(&prog, &args).await.map(|_| ())
    }

    async fn status(&self, handle: &BoxHandle) -> Result<BoxStatus, SandboxError> {
        let power = self.power(handle).await?;
        // A stopped box is never reachable, and probing it would just
        // block on a doomed SSH; only probe when it claims to be running.
        let reachable = if power.is_running() {
            let (prog, args) = self.reachable_probe_command(handle);
            self.run(&prog, &args).await.is_ok()
        } else {
            false
        };
        Ok(BoxStatus { power, reachable })
    }

    async fn connect(&self, handle: &BoxHandle, ports: &[u16]) -> Result<Tunnel, SandboxError> {
        // Wake-on-connect: a stopped box is started before the forward is
        // handed back. Only the power state matters here — the client's
        // keepalive supervisor retries the forward until SSH comes up, so
        // connect need not pay for the reachability probe.
        if !self.power(handle).await?.is_running() {
            self.start(handle).await?;
        }
        Ok(self.connect_tunnel(handle, ports))
    }

    async fn destroy(&self, handle: &BoxHandle) -> Result<(), SandboxError> {
        let (prog, args) = self.terraform_command("destroy", &Self::destroy_vars(handle));
        self.run(&prog, &args).await.map(|_| ())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Deployment;
    use std::collections::VecDeque;
    use std::sync::Mutex;

    fn provider() -> GcpProvider {
        GcpProvider {
            terraform_dir: PathBuf::from("/repo/terraform/sandbox/gcp"),
            state_file: PathBuf::from("/state/lazybox-sbx-abc/terraform.tfstate"),
            user: Some("me".into()),
            remote_socket: "/home/me/.lazybox/run/daemon.sock".into(),
            local_socket: PathBuf::from("/tmp/lazybox.sock"),
            runner: Arc::new(SystemRunner),
            auth: GcpAuth::default(),
        }
    }

    /// One scripted step: the stdout/err a queued invocation returns.
    enum Step {
        Out(String),
        Fail,
    }

    /// Returns queued outputs in order and records every `(program, args,
    /// env)` so a sequencing test can assert both the commands run and their
    /// order, plus the credential env injected into each.
    #[derive(Debug)]
    struct ScriptedRunner {
        queue: Mutex<VecDeque<Result<String, ()>>>,
        calls: Mutex<Vec<(String, Vec<String>, Vec<(String, String)>)>>,
    }

    impl ScriptedRunner {
        fn new(steps: Vec<Step>) -> Arc<Self> {
            let queue = steps
                .into_iter()
                .map(|s| match s {
                    Step::Out(s) => Ok(s),
                    Step::Fail => Err(()),
                })
                .collect();
            Arc::new(Self {
                queue: Mutex::new(queue),
                calls: Mutex::new(Vec::new()),
            })
        }

        fn calls(&self) -> Vec<(String, Vec<String>, Vec<(String, String)>)> {
            self.calls.lock().expect("calls lock").clone()
        }
    }

    impl CommandRunner for ScriptedRunner {
        fn run<'a>(
            &'a self,
            program: &'a str,
            args: &'a [String],
            env: &'a [(String, String)],
        ) -> CommandFuture<'a, String> {
            self.calls.lock().expect("calls lock").push((
                program.to_string(),
                args.to_vec(),
                env.to_vec(),
            ));
            let step = self
                .queue
                .lock()
                .expect("queue lock")
                .pop_front()
                .expect("unexpected extra command");
            Box::pin(async move {
                step.map_err(|()| SandboxError::Command {
                    program: program.to_string(),
                    status: "exit status: 1".to_string(),
                    stderr: "scripted failure".to_string(),
                })
            })
        }
    }

    fn with_runner(runner: Arc<dyn CommandRunner>) -> GcpProvider {
        let mut p = provider();
        p.runner = runner;
        p
    }

    /// The action of a `gcloud compute …` call — `instances <verb>` yields
    /// the verb (`describe`/`start`/…), `ssh` yields `"ssh"`. Used to assert
    /// lifecycle sequencing across calls.
    fn gcloud_action(call: &(String, Vec<String>, Vec<(String, String)>)) -> Option<&str> {
        let (prog, args, _env) = call;
        if prog != "gcloud" || args.first().map(String::as_str) != Some("compute") {
            return None;
        }
        match args.get(1).map(String::as_str) {
            Some("instances") => args.get(2).map(String::as_str),
            Some("ssh") => Some("ssh"),
            other => other,
        }
    }

    fn spec() -> SandboxSpec {
        SandboxSpec {
            provider: "gcp".into(),
            name: "lazybox-sbx-abc".into(),
            project: "proj".into(),
            region: "us-central1".into(),
            zone: "us-central1-a".into(),
            deployment: Deployment::default_recipe().expect("default recipe"),
            install_lazybox: true,
            lazybox_git_sha: String::new(),
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
    fn terraform_ops_pin_the_isolated_state_file() {
        // apply, destroy, and output must all target this box's own state,
        // or two worktrees sharing a module dir clobber each other.
        let p = provider();
        let state = "-state=/state/lazybox-sbx-abc/terraform.tfstate".to_string();
        for (_, args) in [
            p.terraform_command("apply", &[]),
            p.terraform_command("destroy", &[]),
            p.output_command(),
        ] {
            assert!(args.contains(&state), "missing isolated -state: {args:?}");
        }
    }

    #[test]
    fn init_runs_against_the_module_dir() {
        // ensure() inits before apply so a fresh module dir with no
        // providers doesn't hard-fail. init carries no -state/-var.
        let (prog, args) = provider().init_command();
        assert_eq!(prog, "terraform");
        assert_eq!(&args[0], "-chdir=/repo/terraform/sandbox/gcp");
        assert!(args.contains(&"init".to_string()));
        assert!(!args.iter().any(|a| a.starts_with("-state=")), "{args:?}");
        assert!(!args.contains(&"-var".to_string()), "{args:?}");
    }

    #[test]
    fn reachability_probe_is_a_bounded_iap_ssh() {
        let (prog, args) = provider().reachable_probe_command(&handle());
        assert_eq!(prog, "gcloud");
        assert_eq!(args[..3], ["compute", "ssh", "me@lazybox-sbx-abc"]);
        assert!(args.contains(&"--tunnel-through-iap".to_string()));
        // Runs a trivial command and caps the wait so `status` stays snappy.
        assert!(args.contains(&"--command=true".to_string()));
        assert!(args.contains(&format!("ConnectTimeout={PROBE_CONNECT_TIMEOUT}")));
        assert!(args.contains(&"BatchMode=yes".to_string()));
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

    /// The `--command=…` remote string a rebuild ssh carries.
    fn rebuild_remote(args: &[String]) -> String {
        args.iter()
            .find_map(|a| a.strip_prefix("--command="))
            .expect("a --command= arg")
            .to_string()
    }

    #[test]
    fn rebuild_runs_the_on_box_helper_at_the_pinned_sha_over_iap() {
        let (prog, args) = provider().rebuild_command(&handle(), "deadbeef1234");
        assert_eq!(prog, "gcloud");
        assert_eq!(args[..3], ["compute", "ssh", "me@lazybox-sbx-abc"]);
        assert!(args.contains(&"--tunnel-through-iap".to_string()));
        assert!(args.contains(&"--zone=us-central1-a".to_string()));
        let remote = rebuild_remote(&args);
        // Runs the installed helper as root with the SHA…
        assert!(remote.contains("sudo"), "{remote}");
        assert!(
            remote.contains("/usr/local/bin/lazybox-build.sh deadbeef1234"),
            "{remote}"
        );
        // …but NOT in the SSH session's process tree: a dropped IAP link would
        // SIGHUP a 10-minute build (#903). It must run in a supervised systemd
        // transient unit that survives the disconnect, with `--wait` so the
        // exit status still propagates while connected.
        assert!(remote.contains("systemd-run"), "{remote}");
        assert!(remote.contains("--wait"), "{remote}");
        // And it must source the same build.env the boot build reads, so a box
        // provisioned with non-default paths rebuilds against its real config.
        assert!(
            remote.contains("EnvironmentFile=-/etc/lazybox/build.env"),
            "{remote}"
        );
        // Unattended: never block on a passphrase or host-key prompt.
        assert!(args.contains(&"BatchMode=yes".to_string()));
    }

    #[test]
    fn rebuild_with_an_empty_sha_tracks_the_default_branch() {
        // A client with no baked SHA passes empty; the helper then tracks the
        // default branch tip rather than `git checkout`-ing a bogus commit.
        let (_, args) = provider().rebuild_command(&handle(), "");
        let remote = rebuild_remote(&args);
        assert!(remote.trim_end().ends_with("lazybox-build.sh"), "{remote}");
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

    #[tokio::test]
    async fn ensure_runs_init_then_apply_then_output() {
        let outputs = r#"{
            "instance_name": {"value": "lazybox-sbx-abc"},
            "zone": {"value": "us-central1-b"}
        }"#;
        let runner = ScriptedRunner::new(vec![
            Step::Out(String::new()),       // init
            Step::Out(String::new()),       // apply
            Step::Out(outputs.to_string()), // output -json
        ]);
        let provider = with_runner(runner.clone());

        let handle = provider.ensure(&spec()).await.unwrap();

        assert_eq!(handle.id, "lazybox-sbx-abc");
        assert_eq!(handle.zone, "us-central1-b");
        assert_eq!(handle.power_state, PowerState::Running);

        let calls = runner.calls();
        assert_eq!(calls.len(), 3, "ensure runs init→apply→output");
        assert!(calls.iter().all(|(prog, _, _)| prog == "terraform"));
        assert!(calls[0].1.contains(&"init".to_string()));
        assert!(calls[1].1.contains(&"apply".to_string()));
        assert!(calls[2].1.contains(&"output".to_string()));
    }

    #[tokio::test]
    async fn connect_starts_a_stopped_box_before_forwarding() {
        // describe → STOPPED, so connect must start the box, then hand back
        // the forward without a second describe.
        let runner = ScriptedRunner::new(vec![
            Step::Out("TERMINATED".to_string()), // power probe
            Step::Out(String::new()),            // instances start
        ]);
        let provider = with_runner(runner.clone());

        let tunnel = provider.connect(&handle(), &[3000]).await.unwrap();

        assert_eq!(tunnel.program, "gcloud");
        let calls = runner.calls();
        assert_eq!(calls.len(), 2, "power probe, then start");
        assert_eq!(gcloud_action(&calls[0]), Some("describe"));
        assert_eq!(gcloud_action(&calls[1]), Some("start"));
    }

    #[tokio::test]
    async fn connect_skips_start_when_the_box_is_running() {
        let runner = ScriptedRunner::new(vec![Step::Out("RUNNING".to_string())]);
        let provider = with_runner(runner.clone());

        provider.connect(&handle(), &[]).await.unwrap();

        let calls = runner.calls();
        assert_eq!(calls.len(), 1, "a running box needs only the power probe");
        assert_eq!(gcloud_action(&calls[0]), Some("describe"));
    }

    #[tokio::test]
    async fn status_probes_reachability_only_when_running() {
        // Running box: describe says RUNNING, then the reachability SSH is
        // attempted — here it succeeds, so `reachable` is true.
        let runner = ScriptedRunner::new(vec![
            Step::Out("RUNNING".to_string()), // power probe
            Step::Out(String::new()),         // reachability ssh
        ]);
        let provider = with_runner(runner.clone());

        let status = provider.status(&handle()).await.unwrap();
        assert_eq!(status.power, PowerState::Running);
        assert!(status.reachable);

        let calls = runner.calls();
        assert_eq!(calls.len(), 2);
        assert_eq!(gcloud_action(&calls[0]), Some("describe"));
        assert_eq!(gcloud_action(&calls[1]), Some("ssh"));
    }

    #[tokio::test]
    async fn status_of_a_stopped_box_never_probes() {
        // A stopped box is never reachable; probing would block on a doomed
        // SSH, so `status` must not attempt it.
        let runner = ScriptedRunner::new(vec![Step::Out("TERMINATED".to_string())]);
        let provider = with_runner(runner.clone());

        let status = provider.status(&handle()).await.unwrap();
        assert_eq!(status.power, PowerState::Stopped);
        assert!(!status.reachable);
        assert_eq!(
            runner.calls().len(),
            1,
            "no reachability probe when stopped"
        );
    }

    #[tokio::test]
    async fn status_running_but_unreachable_when_the_probe_fails() {
        let runner = ScriptedRunner::new(vec![
            Step::Out("RUNNING".to_string()), // power probe
            Step::Fail,                       // reachability ssh fails
        ]);
        let provider = with_runner(runner);

        let status = provider.status(&handle()).await.unwrap();
        assert_eq!(status.power, PowerState::Running);
        assert!(!status.reachable, "a failed probe reads as unreachable");
    }

    fn env_of(call: &(String, Vec<String>, Vec<(String, String)>)) -> Vec<(String, String)> {
        call.2.clone()
    }

    #[test]
    fn ambient_auth_injects_no_env() {
        // Nothing configured → the legacy path: not a single env override, so
        // the box lifecycle behaves exactly as before #1047.
        let auth = GcpAuth::default();
        assert!(auth.is_ambient());
        assert!(auth.command_env().is_empty());
    }

    #[test]
    fn service_account_key_injects_gcloud_and_terraform_creds() {
        let auth = GcpAuth {
            service_account_key: Some(PathBuf::from("/keys/sa.json")),
            impersonate_service_account: None,
            config_dir: Some(PathBuf::from("/scoped/gcloud")),
        };
        assert!(!auth.is_ambient());
        let env = auth.command_env();
        // Isolated gcloud config so the user's own is never touched…
        assert!(env.contains(&("CLOUDSDK_CONFIG".into(), "/scoped/gcloud".into())));
        // …gcloud reads the key via the override…
        assert!(env.contains(&(
            "CLOUDSDK_AUTH_CREDENTIAL_FILE_OVERRIDE".into(),
            "/keys/sa.json".into()
        )));
        // …and terraform's google provider via GOOGLE_APPLICATION_CREDENTIALS.
        assert!(env.contains(&(
            "GOOGLE_APPLICATION_CREDENTIALS".into(),
            "/keys/sa.json".into()
        )));
    }

    #[test]
    fn impersonation_without_a_key_sets_the_impersonate_env_but_does_not_scope() {
        // Impersonation whose base is ambient/metadata must NOT scope
        // CLOUDSDK_CONFIG: an empty scoped dir has no base credential, so the
        // token exchange could never resolve one (#1047 review finding #2).
        let auth = GcpAuth {
            service_account_key: None,
            impersonate_service_account: Some("deploy@p.iam.gserviceaccount.com".into()),
            config_dir: Some(PathBuf::from("/scoped/gcloud")),
        };
        let env = auth.command_env();
        assert!(env.contains(&(
            "CLOUDSDK_AUTH_IMPERSONATE_SERVICE_ACCOUNT".into(),
            "deploy@p.iam.gserviceaccount.com".into()
        )));
        assert!(env.contains(&(
            "GOOGLE_IMPERSONATE_SERVICE_ACCOUNT".into(),
            "deploy@p.iam.gserviceaccount.com".into()
        )));
        assert!(
            !env.iter().any(|(k, _)| k == "CLOUDSDK_CONFIG"),
            "must not strand impersonation's base by scoping: {env:?}"
        );
    }

    #[test]
    fn impersonation_with_a_key_scopes_off_that_base_key() {
        // A key IS a base credential, so scoping is safe and desirable here.
        let auth = GcpAuth {
            service_account_key: Some(PathBuf::from("/keys/sa.json")),
            impersonate_service_account: Some("deploy@p.iam.gserviceaccount.com".into()),
            config_dir: Some(PathBuf::from("/scoped/gcloud")),
        };
        let env = auth.command_env();
        assert!(env.contains(&("CLOUDSDK_CONFIG".into(), "/scoped/gcloud".into())));
        assert!(env.contains(&(
            "GOOGLE_IMPERSONATE_SERVICE_ACCOUNT".into(),
            "deploy@p.iam.gserviceaccount.com".into()
        )));
    }

    #[tokio::test]
    async fn injected_credentials_ride_a_real_provider_op() {
        // The #1047 core: credentials ride *every* gcloud/terraform call, not
        // just the preflight. `status` of a stopped box is one describe call —
        // it must carry the injected override.
        let runner = ScriptedRunner::new(vec![Step::Out("TERMINATED".to_string())]);
        let mut provider = with_runner(runner.clone());
        provider.auth = GcpAuth {
            service_account_key: Some(PathBuf::from("/keys/sa.json")),
            impersonate_service_account: None,
            config_dir: Some(PathBuf::from("/scoped/gcloud")),
        };

        provider.status(&handle()).await.unwrap();

        let calls = runner.calls();
        assert_eq!(calls.len(), 1, "a stopped box is one describe call");
        let env = env_of(&calls[0]);
        assert!(env.contains(&(
            "CLOUDSDK_AUTH_CREDENTIAL_FILE_OVERRIDE".into(),
            "/keys/sa.json".into()
        )));
        assert!(env.contains(&("CLOUDSDK_CONFIG".into(), "/scoped/gcloud".into())));
    }

    #[tokio::test]
    async fn check_auth_with_a_key_validates_offline_without_a_token_probe() {
        // A service-account key is long-lived; check_auth validates it by
        // reading the file, spawning no gcloud — so the snappy `status` never
        // pays a network token mint (#1047 review finding #3).
        let key = std::env::temp_dir().join("lazybox-1047-sa-offline.json");
        std::fs::write(&key, b"{}").expect("write fake key");
        let runner = ScriptedRunner::new(vec![]);
        let mut provider = with_runner(runner.clone());
        provider.auth = GcpAuth {
            service_account_key: Some(key.clone()),
            impersonate_service_account: None,
            config_dir: None,
        };

        provider
            .check_auth()
            .await
            .expect("a readable key passes preflight offline");
        assert!(
            runner.calls().is_empty(),
            "no token probe when a key is configured"
        );
        std::fs::remove_file(&key).ok();
    }

    #[tokio::test]
    async fn check_auth_flags_an_unreadable_key_before_spawning_gcloud() {
        // A configured-but-absent/unreadable key is the common misconfiguration;
        // it must fail with a precise message and never reach the runner. A bad
        // path exercises the unreadable branch (#1047 review finding #4).
        let runner = ScriptedRunner::new(vec![]);
        let mut provider = with_runner(runner.clone());
        provider.auth = GcpAuth {
            service_account_key: Some(PathBuf::from("/definitely/not/here.json")),
            impersonate_service_account: None,
            config_dir: None,
        };

        let err = provider.check_auth().await.unwrap_err();
        assert!(matches!(err, SandboxError::Config(_)), "{err:?}");
        assert!(err.to_string().contains("unreadable"), "{err}");
        assert!(runner.calls().is_empty(), "no gcloud spawn on a bad path");
    }

    #[tokio::test]
    async fn check_auth_probes_only_the_no_key_path_and_maps_failure_to_a_fix_hint() {
        // Ambient path (no offline-checkable base): the probe fails (no login)
        // → the actionable "configure credentials or run gcloud auth login"
        // message, not raw stderr.
        let runner = ScriptedRunner::new(vec![Step::Fail]);
        let provider = with_runner(runner.clone());
        let err = provider.check_auth().await.unwrap_err();
        assert!(matches!(err, SandboxError::Config(_)), "{err:?}");
        assert!(err.to_string().contains("not configured"), "{err}");
        assert_eq!(
            runner.calls().len(),
            1,
            "the no-key path probes exactly once"
        );
        assert_eq!(runner.calls()[0].1, vec!["auth", "print-access-token"]);
    }

    #[test]
    fn connect_tunnel_carries_the_injected_credentials() {
        // The forward is spawned outside the CommandRunner (by the client's
        // keepalive supervisor), so the tunnel itself must carry the creds or
        // the IAP SSH falls back to ambient auth (#1047 review finding #1).
        // Ambient → no env on the tunnel, exactly as before #1047.
        assert!(provider().connect_tunnel(&handle(), &[]).env.is_empty());

        let mut authed = provider();
        authed.auth = GcpAuth {
            service_account_key: Some(PathBuf::from("/keys/sa.json")),
            impersonate_service_account: None,
            config_dir: Some(PathBuf::from("/scoped/gcloud")),
        };
        let tunnel = authed.connect_tunnel(&handle(), &[3000]);
        assert!(tunnel.env.contains(&(
            "CLOUDSDK_AUTH_CREDENTIAL_FILE_OVERRIDE".into(),
            "/keys/sa.json".into()
        )));
        assert!(
            tunnel
                .env
                .contains(&("CLOUDSDK_CONFIG".into(), "/scoped/gcloud".into()))
        );
    }

    #[test]
    fn auth_failure_message_depends_on_whether_creds_were_configured() {
        let ambient = auth_failure_message(&GcpAuth::default(), "boom");
        assert!(ambient.contains("gcloud auth login"), "{ambient}");
        let configured = auth_failure_message(
            &GcpAuth {
                service_account_key: Some(PathBuf::from("/k.json")),
                ..GcpAuth::default()
            },
            "boom",
        );
        assert!(configured.contains("expired"), "{configured}");
    }
}
