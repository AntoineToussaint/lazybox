use std::path::PathBuf;

use crate::{BoxHandle, BoxStatus, SandboxSpec};

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
}

/// Provider-agnostic box lifecycle.
///
/// Split by cost: `ensure`/`destroy` drive a Terraform module (create /
/// tear down infrastructure); `start`/`stop`/`status`/`connect` use the
/// native CLI so waking a box is fast and cheap. Implementations are
/// selected at compile time behind a Cargo feature (`gcp` first), so the
/// trait needs no object safety — the async methods stay ergonomic.
#[allow(async_fn_in_trait)]
pub trait SandboxProvider {
    /// Provider id, e.g. `"gcp"`.
    fn id(&self) -> &str;

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
