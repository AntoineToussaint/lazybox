use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::process::Stdio;

use tokio::process::Command;

use crate::{BoxHandle, BoxStatus, SandboxSpec};

/// Reject a handle before it crosses into a different provider's API.
pub fn validate_handle_provider(
    expected_provider: &str,
    handle: &BoxHandle,
) -> Result<(), SandboxError> {
    if handle.provider == expected_provider {
        return Ok(());
    }
    Err(SandboxError::Config(format!(
        "box {} belongs to sandbox provider {:?}, but provider {:?} is selected; select the handle's provider before managing or replacing it",
        handle.id, handle.provider, expected_provider
    )))
}

/// A ready-to-spawn port-forward for a connected box.
///
/// `connect` returns the forward invocation rather than owning the
/// long-lived process: the in-process keepalive supervisor already lives
/// in the client (`tui-boot::tunnel`, #908), so the provider's job ends at
/// "here is the exact `gcloud`/`ssh` command that binds these ports". The
/// forward carries the daemon Unix socket (Unix→Unix) plus each workload
/// TCP port bound to `localhost` on the client — so a browser hitting
/// `localhost:3000` needs no public host and no auth-allowlist change.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Tunnel {
    /// Program to spawn (`gcloud`, `ssh`).
    pub program: String,
    pub args: Vec<String>,
    /// Credentials overlaid on the forward's environment (#1047). The
    /// long-lived `gcloud compute ssh` that carries the daemon socket is
    /// spawned by the *caller* (the client's keepalive supervisor), not
    /// through the provider's `CommandRunner`, so the auth env must ride the
    /// tunnel to reach it — otherwise the forward falls back to ambient
    /// credentials the rest of the lifecycle no longer uses. Empty on the
    /// ambient path.
    pub env: Vec<(String, String)>,
    /// Local socket the forward binds; the path `--connect` should dial.
    pub local_socket: PathBuf,
    /// Workload TCP ports forwarded to `localhost` on the client.
    pub ports: Vec<u16>,
}

/// Failures a provider surfaces. Command failures keep the program name and
/// captured stderr so a broken `terraform`/`gcloud` invocation is
/// diagnosable rather than a bare exit code.
#[derive(Debug, thiserror::Error)]
pub enum SandboxError {
    #[error("spawn `{program}`: {source}")]
    Spawn {
        program: String,
        #[source]
        source: std::io::Error,
    },
    #[error("`{program}` exited with {status}: {stderr}")]
    Command {
        program: String,
        status: String,
        stderr: String,
    },
    #[error("parsing {what}: {detail}")]
    Parse { what: &'static str, detail: String },
    #[error("deployment: {0}")]
    Deployment(String),
    #[error(transparent)]
    Store(#[from] lazybox_store::StoreError),
    #[error("serialize handle: {0}")]
    Serialize(String),
    #[error("{0}")]
    Config(String),
    /// The configured credentials are present but no longer valid and the
    /// user must re-authenticate — the `invalid_rapt` / expired-ADC case
    /// (#1126). Distinct from [`Config`](Self::Config) so the UI can render
    /// an actionable "re-authenticate" prompt instead of a generic setup
    /// hint: the fix is a login, not a config edit.
    #[error("gcp credentials expired — re-authenticate: {detail}")]
    ReauthRequired { detail: String },
    #[error("{provider} API {operation}: {detail}")]
    ApiTransport {
        provider: &'static str,
        operation: &'static str,
        detail: String,
    },
    #[error("{provider} API {operation} returned HTTP {status}: {detail}")]
    Api {
        provider: &'static str,
        operation: &'static str,
        status: u16,
        detail: String,
    },
    #[error("{operation}: {source}")]
    Io {
        operation: &'static str,
        #[source]
        source: std::io::Error,
    },
    #[error("{operation}: background task failed: {detail}")]
    Task {
        operation: &'static str,
        detail: String,
    },
}

/// Boxed async result returned by [`CommandRunner`].
pub type CommandFuture<'a, T> = Pin<Box<dyn Future<Output = Result<T, SandboxError>> + Send + 'a>>;

/// Runs a provider CLI command to completion.
pub trait CommandRunner: Send + Sync + std::fmt::Debug {
    fn run<'a>(
        &'a self,
        program: &'a str,
        args: &'a [String],
        env: &'a [(String, String)],
    ) -> CommandFuture<'a, String>;
}

/// The production command runner.
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

/// Provider-agnostic box lifecycle.
///
/// Split by cost: `ensure`/`destroy` drive a Terraform module (create /
/// tear down infrastructure); `start`/`stop`/`status`/`connect` use the
/// native CLI so waking a box is fast and cheap. Implementations are
/// selected at compile time behind Cargo features, so the
/// trait needs no object safety — the async methods stay ergonomic.
#[allow(async_fn_in_trait)]
pub trait SandboxProvider {
    /// Provider id, e.g. `"gcp"`.
    fn id(&self) -> &str;

    /// Verify the provider's own credentials before the first lifecycle op,
    /// failing with an actionable message (`gcp credentials not configured /
    /// expired: <how to fix>`) rather than a raw CLI stderr from deep inside
    /// a later call. Auth is a per-provider concern (GCP = service-account
    /// key / impersonation, E2B = API key, …), so it lives on the contract
    /// (#1047). The default is a no-op: a provider whose credentials are
    /// implicit needs no preflight.
    async fn check_auth(&self) -> Result<(), SandboxError> {
        Ok(())
    }

    /// Create the box if it is absent (Terraform apply), returning a handle
    /// that later lifecycle ops address. Idempotent: a second `ensure` of
    /// the same spec converges rather than duplicating.
    async fn ensure(&self, spec: &SandboxSpec) -> Result<BoxHandle, SandboxError>;

    /// Wake a stopped box (native start).
    async fn start(&self, handle: &BoxHandle) -> Result<(), SandboxError>;

    /// Put a box to sleep (native stop) — the sleep half of the idle
    /// policy (#913). A stopped box costs nothing.
    async fn stop(&self, handle: &BoxHandle) -> Result<(), SandboxError>;

    /// Probe power state and reachability without waking the box.
    async fn status(&self, handle: &BoxHandle) -> Result<BoxStatus, SandboxError>;

    /// Build the port-forward for `ports` (plus the daemon socket). Wakes
    /// the box first if it is stopped — connect is also wake-on-connect.
    async fn connect(&self, handle: &BoxHandle, ports: &[u16]) -> Result<Tunnel, SandboxError>;

    /// Tear the box down (Terraform destroy).
    async fn destroy(&self, handle: &BoxHandle) -> Result<(), SandboxError>;
}
