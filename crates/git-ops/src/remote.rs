//! Remote-host targeting: provision and lifecycle a per-worktree box.
//!
//! `git-ops` owns the worktree; a *remote* worktree runs on a box we
//! stamp from a golden image instead of on the daemon host. This module
//! drives `gcloud compute` to reuse-or-create that box, stop it on idle,
//! and delete it on cleanup. It is source-agnostic and store-free — the
//! caller derives the instance name from the worktree, persists the
//! returned [`RemoteHost`] handle, and hands it back for stop/teardown.
//!
//! The `gcloud` invocations run behind [`GcloudRunner`], so the
//! provisioner is exercised in full without a Google Cloud project.

use std::future::Future;
use std::pin::Pin;
use std::process::Output;
use std::sync::Arc;
use std::time::Duration;

use serde::{Deserialize, Serialize};

/// How long a single `gcloud` invocation may run before it is abandoned.
/// Instance create/start/stop each block on the operation completing, so
/// this is generous.
const GCLOUD_TIMEOUT: Duration = Duration::from_secs(180);

#[derive(Debug, thiserror::Error)]
pub enum RemoteError {
    #[error("gcloud failed: {0}")]
    Gcloud(String),
    #[error("gcloud I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("could not parse gcloud output: {0}")]
    Parse(String),
    #[error("remote host spec is incomplete: set either source_image or instance_template")]
    MissingImage,
}

/// A provisioned box, persisted against the worktree that stamped it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RemoteHost {
    pub project: String,
    pub zone: String,
    pub instance: String,
    /// GCE numeric instance id. Stable for the life of the box and
    /// unique across a delete-and-recreate of the same name, so it
    /// pins the handle to the exact box the name resolved to.
    pub id: String,
}

/// Inputs for stamping a box from the golden image.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteHostSpec {
    pub project: String,
    pub zone: String,
    pub machine_type: String,
    /// A `--source-machine-image` to stamp from. Ignored when
    /// `instance_template` is set.
    pub source_image: Option<String>,
    /// A `--source-instance-template` to stamp from. Wins over
    /// `source_image` when both are present.
    pub instance_template: Option<String>,
}

/// Where a box sits in its lifecycle, parsed from gcloud's `status`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HostState {
    Provisioning,
    Running,
    Stopping,
    Stopped,
    Suspended,
    Terminated,
    Unknown(String),
}

impl HostState {
    fn from_gcloud(status: &str) -> Self {
        match status {
            "PROVISIONING" | "STAGING" | "REPAIRING" => Self::Provisioning,
            "RUNNING" => Self::Running,
            "STOPPING" => Self::Stopping,
            "STOPPED" => Self::Stopped,
            "SUSPENDING" | "SUSPENDED" => Self::Suspended,
            "TERMINATED" => Self::Terminated,
            other => Self::Unknown(other.to_string()),
        }
    }

    /// A box that must be started before it can accept work again.
    pub fn needs_start(&self) -> bool {
        matches!(self, Self::Stopped | Self::Suspended | Self::Terminated)
    }
}

/// Boxed async result returned by [`GcloudRunner`].
pub type GcloudFuture<'a, T> = Pin<Box<dyn Future<Output = Result<T, RemoteError>> + Send + 'a>>;

/// Runs one `gcloud` invocation. Injectable so the provisioner's
/// reuse/create/teardown logic is unit-tested against scripted output
/// without a real cloud project — see this module's tests.
pub trait GcloudRunner: Send + Sync {
    /// Run `gcloud` with `args`. A non-zero exit is returned in the
    /// [`Output`]; only spawn/wait/timeout failures are errors, so the
    /// provisioner can read gcloud's exit status (a "not found" is an
    /// absent box, not a failure).
    fn run<'a>(&'a self, args: &'a [&'a str]) -> GcloudFuture<'a, Output>;
}

/// The production runner: shells out to `gcloud` with a bounded timeout.
pub struct SystemGcloud;

impl GcloudRunner for SystemGcloud {
    fn run<'a>(&'a self, args: &'a [&'a str]) -> GcloudFuture<'a, Output> {
        Box::pin(async move {
            let mut cmd = tokio::process::Command::new("gcloud");
            cmd.args(args).stdin(std::process::Stdio::null());
            match tokio::time::timeout(GCLOUD_TIMEOUT, cmd.output()).await {
                Ok(result) => Ok(result?),
                Err(_) => Err(RemoteError::Gcloud(format!(
                    "gcloud {} timed out after {}s",
                    args.join(" "),
                    GCLOUD_TIMEOUT.as_secs()
                ))),
            }
        })
    }
}

/// Instance metadata read back from `gcloud compute instances describe`.
struct InstanceInfo {
    id: String,
    state: HostState,
}

/// Drives `gcloud compute` to reuse-or-provision, stop, and tear down a
/// per-worktree box.
pub struct GcloudProvisioner {
    runner: Arc<dyn GcloudRunner>,
}

impl Default for GcloudProvisioner {
    fn default() -> Self {
        Self::new()
    }
}

impl GcloudProvisioner {
    pub fn new() -> Self {
        Self {
            runner: Arc::new(SystemGcloud),
        }
    }

    pub fn with_runner(runner: Arc<dyn GcloudRunner>) -> Self {
        Self { runner }
    }

    /// Reuse the worktree's box if it already exists — starting it first
    /// when it was stopped on idle — otherwise stamp a fresh one from the
    /// golden image. The instance name is the worktree's stable handle,
    /// so a describe hit *is* the reuse; there is no separate registry to
    /// consult.
    pub async fn ensure(
        &self,
        instance: &str,
        spec: &RemoteHostSpec,
    ) -> Result<RemoteHost, RemoteError> {
        if let Some(info) = self.describe(&spec.project, &spec.zone, instance).await? {
            if info.state.needs_start() {
                self.start_instance(&spec.project, &spec.zone, instance)
                    .await?;
            }
            return Ok(self.handle(spec, instance, info.id));
        }
        self.create(instance, spec).await?;
        let info = self
            .describe(&spec.project, &spec.zone, instance)
            .await?
            .ok_or_else(|| {
                RemoteError::Gcloud(format!("instance {instance} missing after create"))
            })?;
        Ok(self.handle(spec, instance, info.id))
    }

    /// Stop the box (stop-on-idle). The boot disk persists, so a later
    /// [`ensure`](Self::ensure) starts and reuses it.
    pub async fn stop(&self, host: &RemoteHost) -> Result<(), RemoteError> {
        self.run_ok(&[
            "compute",
            "instances",
            "stop",
            &host.instance,
            "--project",
            &host.project,
            "--zone",
            &host.zone,
        ])
        .await
    }

    /// Delete the box (teardown on worktree cleanup). Irreversible. An
    /// already-absent box is treated as success so a repeated cleanup —
    /// or one after a manual `gcloud delete` — does not error.
    pub async fn teardown(&self, host: &RemoteHost) -> Result<(), RemoteError> {
        let out = self
            .runner
            .run(&[
                "compute",
                "instances",
                "delete",
                &host.instance,
                "--project",
                &host.project,
                "--zone",
                &host.zone,
                "--quiet",
            ])
            .await?;
        if out.status.success() || is_not_found(&out.stderr) {
            Ok(())
        } else {
            Err(RemoteError::Gcloud(stderr_string(&out.stderr)))
        }
    }

    /// Current lifecycle state, or `None` when the box no longer exists.
    pub async fn status(&self, host: &RemoteHost) -> Result<Option<HostState>, RemoteError> {
        Ok(self
            .describe(&host.project, &host.zone, &host.instance)
            .await?
            .map(|info| info.state))
    }

    fn handle(&self, spec: &RemoteHostSpec, instance: &str, id: String) -> RemoteHost {
        RemoteHost {
            project: spec.project.clone(),
            zone: spec.zone.clone(),
            instance: instance.to_string(),
            id,
        }
    }

    async fn describe(
        &self,
        project: &str,
        zone: &str,
        instance: &str,
    ) -> Result<Option<InstanceInfo>, RemoteError> {
        let out = self
            .runner
            .run(&[
                "compute",
                "instances",
                "describe",
                instance,
                "--project",
                project,
                "--zone",
                zone,
                "--format=json",
            ])
            .await?;
        if out.status.success() {
            return Ok(Some(parse_instance(&out.stdout)?));
        }
        if is_not_found(&out.stderr) {
            return Ok(None);
        }
        Err(RemoteError::Gcloud(stderr_string(&out.stderr)))
    }

    async fn create(&self, instance: &str, spec: &RemoteHostSpec) -> Result<(), RemoteError> {
        let mut args: Vec<&str> = vec![
            "compute",
            "instances",
            "create",
            instance,
            "--project",
            &spec.project,
            "--zone",
            &spec.zone,
            "--machine-type",
            &spec.machine_type,
        ];
        if let Some(template) = &spec.instance_template {
            args.push("--source-instance-template");
            args.push(template);
        } else if let Some(image) = &spec.source_image {
            args.push("--source-machine-image");
            args.push(image);
        } else {
            return Err(RemoteError::MissingImage);
        }
        self.run_ok(&args).await
    }

    async fn start_instance(
        &self,
        project: &str,
        zone: &str,
        instance: &str,
    ) -> Result<(), RemoteError> {
        self.run_ok(&[
            "compute",
            "instances",
            "start",
            instance,
            "--project",
            project,
            "--zone",
            zone,
        ])
        .await
    }

    async fn run_ok(&self, args: &[&str]) -> Result<(), RemoteError> {
        let out = self.runner.run(args).await?;
        if out.status.success() {
            Ok(())
        } else {
            Err(RemoteError::Gcloud(stderr_string(&out.stderr)))
        }
    }
}

fn parse_instance(stdout: &[u8]) -> Result<InstanceInfo, RemoteError> {
    #[derive(Deserialize)]
    struct Raw {
        id: String,
        status: String,
    }
    let raw: Raw =
        serde_json::from_slice(stdout).map_err(|error| RemoteError::Parse(error.to_string()))?;
    Ok(InstanceInfo {
        id: raw.id,
        state: HostState::from_gcloud(&raw.status),
    })
}

fn is_not_found(stderr: &[u8]) -> bool {
    let text = String::from_utf8_lossy(stderr);
    text.contains("was not found") || text.contains("404")
}

fn stderr_string(stderr: &[u8]) -> String {
    String::from_utf8_lossy(stderr).trim().to_string()
}

/// Build a GCE-valid instance name from a prefix and a worktree
/// identifier. GCE names match `[a-z]([-a-z0-9]*[a-z0-9])?` and are at
/// most 63 characters: lowercase, every other run collapses to a single
/// `-`, a leading non-letter gets an `lb-` prefix, and the result is
/// trimmed to fit. Deterministic, so the same worktree always maps to
/// the same box — the reuse key in [`GcloudProvisioner::ensure`].
pub fn instance_name(prefix: &str, worktree_key: &str) -> String {
    let mut name = String::new();
    let mut prev_dash = false;
    for ch in format!("{prefix}-{worktree_key}").chars() {
        if ch.is_ascii_alphanumeric() {
            name.push(ch.to_ascii_lowercase());
            prev_dash = false;
        } else if !prev_dash {
            name.push('-');
            prev_dash = true;
        }
    }
    let trimmed = name.trim_matches('-');
    let mut name = if trimmed.starts_with(|c: char| c.is_ascii_lowercase()) {
        trimmed.to_string()
    } else {
        format!("lb-{trimmed}")
    };
    name.truncate(63);
    name.trim_end_matches('-').to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;
    use std::os::unix::process::ExitStatusExt;
    use std::process::ExitStatus;
    use std::sync::Mutex;

    fn ok(stdout: &str) -> Output {
        Output {
            status: ExitStatus::from_raw(0),
            stdout: stdout.as_bytes().to_vec(),
            stderr: Vec::new(),
        }
    }

    fn err(stderr: &str) -> Output {
        Output {
            status: ExitStatus::from_raw(1 << 8),
            stdout: Vec::new(),
            stderr: stderr.as_bytes().to_vec(),
        }
    }

    fn describe_json(id: &str, status: &str) -> String {
        format!(r#"{{"id":"{id}","status":"{status}","name":"box"}}"#)
    }

    /// Returns queued outputs in order and records every arg vector.
    struct ScriptedGcloud {
        queue: Mutex<VecDeque<Output>>,
        calls: Mutex<Vec<Vec<String>>>,
    }

    impl ScriptedGcloud {
        fn new(outputs: Vec<Output>) -> Arc<Self> {
            Arc::new(Self {
                queue: Mutex::new(outputs.into()),
                calls: Mutex::new(Vec::new()),
            })
        }

        fn calls(&self) -> Vec<Vec<String>> {
            self.calls.lock().unwrap().clone()
        }
    }

    impl GcloudRunner for ScriptedGcloud {
        fn run<'a>(&'a self, args: &'a [&'a str]) -> GcloudFuture<'a, Output> {
            self.calls
                .lock()
                .unwrap()
                .push(args.iter().map(|s| s.to_string()).collect());
            let out = self
                .queue
                .lock()
                .unwrap()
                .pop_front()
                .expect("unexpected extra gcloud call");
            Box::pin(async move { Ok(out) })
        }
    }

    fn spec() -> RemoteHostSpec {
        RemoteHostSpec {
            project: "proj".into(),
            zone: "us-central1-a".into(),
            machine_type: "e2-standard-8".into(),
            source_image: Some("golden".into()),
            instance_template: None,
        }
    }

    fn joined(call: &[String]) -> String {
        call.join(" ")
    }

    #[tokio::test]
    async fn ensure_reuses_running_box_without_creating() {
        let runner = ScriptedGcloud::new(vec![ok(&describe_json("123", "RUNNING"))]);
        let provisioner = GcloudProvisioner::with_runner(runner.clone());

        let host = provisioner.ensure("box", &spec()).await.unwrap();

        assert_eq!(host.id, "123");
        assert_eq!(host.instance, "box");
        assert_eq!(host.project, "proj");
        let calls = runner.calls();
        assert_eq!(calls.len(), 1, "reuse should describe only");
        assert!(joined(&calls[0]).contains("describe"));
    }

    #[tokio::test]
    async fn ensure_starts_a_stopped_box_before_reusing() {
        let runner = ScriptedGcloud::new(vec![ok(&describe_json("55", "STOPPED")), ok("")]);
        let provisioner = GcloudProvisioner::with_runner(runner.clone());

        let host = provisioner.ensure("box", &spec()).await.unwrap();

        assert_eq!(host.id, "55");
        let calls = runner.calls();
        assert_eq!(calls.len(), 2);
        assert!(joined(&calls[0]).contains("describe"));
        assert!(joined(&calls[1]).contains("instances start box"));
    }

    #[tokio::test]
    async fn ensure_creates_from_image_when_absent() {
        let runner = ScriptedGcloud::new(vec![
            err("The resource 'box' was not found"),
            ok(""),
            ok(&describe_json("777", "RUNNING")),
        ]);
        let provisioner = GcloudProvisioner::with_runner(runner.clone());

        let host = provisioner.ensure("box", &spec()).await.unwrap();

        assert_eq!(host.id, "777");
        let calls = runner.calls();
        assert_eq!(calls.len(), 3);
        let create = joined(&calls[1]);
        assert!(create.contains("instances create box"));
        assert!(create.contains("--source-machine-image golden"));
        assert!(create.contains("--machine-type e2-standard-8"));
    }

    #[tokio::test]
    async fn ensure_prefers_instance_template_over_image() {
        let mut spec = spec();
        spec.instance_template = Some("tmpl".into());
        let runner = ScriptedGcloud::new(vec![
            err("was not found"),
            ok(""),
            ok(&describe_json("9", "RUNNING")),
        ]);
        let provisioner = GcloudProvisioner::with_runner(runner.clone());

        provisioner.ensure("box", &spec).await.unwrap();

        let create = joined(&runner.calls()[1]);
        assert!(create.contains("--source-instance-template tmpl"));
        assert!(!create.contains("--source-machine-image"));
    }

    #[tokio::test]
    async fn ensure_errors_when_spec_has_no_image() {
        let mut spec = spec();
        spec.source_image = None;
        let runner = ScriptedGcloud::new(vec![err("was not found")]);
        let provisioner = GcloudProvisioner::with_runner(runner);

        let error = provisioner.ensure("box", &spec).await.unwrap_err();
        assert!(matches!(error, RemoteError::MissingImage));
    }

    #[tokio::test]
    async fn describe_propagates_a_real_error() {
        let runner = ScriptedGcloud::new(vec![err("PERMISSION_DENIED: caller lacks access")]);
        let provisioner = GcloudProvisioner::with_runner(runner);

        let error = provisioner.ensure("box", &spec()).await.unwrap_err();
        assert!(matches!(error, RemoteError::Gcloud(msg) if msg.contains("PERMISSION_DENIED")));
    }

    #[tokio::test]
    async fn stop_issues_gcloud_stop() {
        let runner = ScriptedGcloud::new(vec![ok("")]);
        let provisioner = GcloudProvisioner::with_runner(runner.clone());
        let host = RemoteHost {
            project: "proj".into(),
            zone: "z".into(),
            instance: "box".into(),
            id: "1".into(),
        };

        provisioner.stop(&host).await.unwrap();
        assert!(joined(&runner.calls()[0]).contains("instances stop box"));
    }

    #[tokio::test]
    async fn teardown_deletes_quietly() {
        let runner = ScriptedGcloud::new(vec![ok("")]);
        let provisioner = GcloudProvisioner::with_runner(runner.clone());
        let host = RemoteHost {
            project: "proj".into(),
            zone: "z".into(),
            instance: "box".into(),
            id: "1".into(),
        };

        provisioner.teardown(&host).await.unwrap();
        let call = joined(&runner.calls()[0]);
        assert!(call.contains("instances delete box"));
        assert!(call.contains("--quiet"));
    }

    #[tokio::test]
    async fn teardown_of_an_absent_box_is_success() {
        let runner = ScriptedGcloud::new(vec![err("The resource 'box' was not found")]);
        let provisioner = GcloudProvisioner::with_runner(runner);
        let host = RemoteHost {
            project: "proj".into(),
            zone: "z".into(),
            instance: "box".into(),
            id: "1".into(),
        };

        provisioner.teardown(&host).await.unwrap();
    }

    #[tokio::test]
    async fn status_is_none_when_the_box_is_gone() {
        let runner = ScriptedGcloud::new(vec![err("was not found")]);
        let provisioner = GcloudProvisioner::with_runner(runner);
        let host = RemoteHost {
            project: "proj".into(),
            zone: "z".into(),
            instance: "box".into(),
            id: "1".into(),
        };

        assert_eq!(provisioner.status(&host).await.unwrap(), None);
    }

    #[test]
    fn instance_name_sanitizes_to_gce_rules() {
        assert_eq!(
            instance_name("lazybox", "obin-ai/portal-issue-42"),
            "lazybox-obin-ai-portal-issue-42"
        );
        // Uppercase and other punctuation collapse to single dashes.
        assert_eq!(instance_name("lb", "Feat_X#7"), "lb-feat-x-7");
    }

    #[test]
    fn instance_name_forces_a_leading_letter() {
        assert!(instance_name("", "9-boxes").starts_with(|c: char| c.is_ascii_lowercase()));
    }

    #[test]
    fn instance_name_fits_in_63_chars_without_trailing_dash() {
        let name = instance_name("lazybox", &"x".repeat(200));
        assert!(name.len() <= 63);
        assert!(!name.ends_with('-'));
    }
}
