//! Interface-first provider for a hosted Kubernetes sandbox control plane.
//!
//! The hosted API is deliberately injected through [`ManagedSandboxApi`]:
//! lazybox owns the lifecycle contract now, while the service can choose its
//! Kubernetes implementation without leaking namespaces, pods, or ingress
//! details into the client. [`InMemoryManagedApi`] is the executable fake used
//! by tests and future fleet-conductor work.

use std::collections::HashMap;
use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::{Arc, Mutex, MutexGuard};

use chrono::Utc;

use crate::{
    BoxHandle, BoxStatus, PowerState, SandboxError, SandboxProvider, SandboxSpec, Tunnel,
    validate_handle_provider,
};

/// Stable provider id persisted in [`BoxHandle`] records.
pub const PROVIDER_ID: &str = "managed-k8s";

/// Boxed async result returned by [`ManagedSandboxApi`].
pub type ManagedApiFuture<'a, T> =
    Pin<Box<dyn Future<Output = Result<T, SandboxError>> + Send + 'a>>;

/// Provider-neutral instance returned by the hosted control plane.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManagedInstance {
    pub id: String,
    pub region: String,
    pub project: String,
    pub power_state: PowerState,
}

/// Connection request sent to the hosted control plane.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManagedConnectRequest {
    pub local_socket: PathBuf,
    pub remote_socket: String,
    pub ports: Vec<u16>,
}

/// The narrow API a managed Kubernetes service must implement.
///
/// `ensure` owns namespace/pod creation, `start` and `stop` map to scaling,
/// `connect` returns the service's authenticated ingress/exec tunnel, and
/// `destroy` removes all tenant resources for the instance.
pub trait ManagedSandboxApi: Send + Sync + std::fmt::Debug {
    fn check_auth(&self) -> ManagedApiFuture<'_, ()> {
        Box::pin(async { Ok(()) })
    }

    fn ensure<'a>(&'a self, spec: &'a SandboxSpec) -> ManagedApiFuture<'a, ManagedInstance>;

    fn start<'a>(&'a self, id: &'a str) -> ManagedApiFuture<'a, ()>;

    fn stop<'a>(&'a self, id: &'a str) -> ManagedApiFuture<'a, ()>;

    fn status<'a>(&'a self, id: &'a str) -> ManagedApiFuture<'a, BoxStatus>;

    fn connect<'a>(
        &'a self,
        id: &'a str,
        request: &'a ManagedConnectRequest,
    ) -> ManagedApiFuture<'a, Tunnel>;

    fn destroy<'a>(&'a self, id: &'a str) -> ManagedApiFuture<'a, ()>;
}

/// [`SandboxProvider`] adapter for a hosted Kubernetes control plane.
#[derive(Debug, Clone)]
pub struct ManagedProvider {
    api: Arc<dyn ManagedSandboxApi>,
    remote_socket: String,
    local_socket: PathBuf,
}

impl ManagedProvider {
    /// Bind a control-plane implementation to lazybox's sandbox contract.
    pub fn new(
        api: Arc<dyn ManagedSandboxApi>,
        remote_socket: String,
        local_socket: PathBuf,
    ) -> Self {
        Self {
            api,
            remote_socket,
            local_socket,
        }
    }

    fn validate(&self, handle: &BoxHandle) -> Result<(), SandboxError> {
        validate_handle_provider(PROVIDER_ID, handle)
    }
}

impl SandboxProvider for ManagedProvider {
    fn id(&self) -> &str {
        PROVIDER_ID
    }

    async fn check_auth(&self) -> Result<(), SandboxError> {
        self.api.check_auth().await
    }

    async fn ensure(&self, spec: &SandboxSpec) -> Result<BoxHandle, SandboxError> {
        if spec.provider != PROVIDER_ID {
            return Err(SandboxError::Config(format!(
                "sandbox spec belongs to provider {:?}, but provider {:?} is selected",
                spec.provider, PROVIDER_ID
            )));
        }
        let instance = self.api.ensure(spec).await?;
        Ok(BoxHandle {
            provider: PROVIDER_ID.to_string(),
            id: instance.id,
            region: instance.region,
            zone: String::new(),
            project: instance.project,
            power_state: instance.power_state,
            last_active: instance.power_state.is_running().then(Utc::now),
        })
    }

    async fn start(&self, handle: &BoxHandle) -> Result<(), SandboxError> {
        self.validate(handle)?;
        self.api.start(&handle.id).await
    }

    async fn stop(&self, handle: &BoxHandle) -> Result<(), SandboxError> {
        self.validate(handle)?;
        self.api.stop(&handle.id).await
    }

    async fn status(&self, handle: &BoxHandle) -> Result<BoxStatus, SandboxError> {
        self.validate(handle)?;
        self.api.status(&handle.id).await
    }

    async fn connect(&self, handle: &BoxHandle, ports: &[u16]) -> Result<Tunnel, SandboxError> {
        self.validate(handle)?;
        if !self.api.status(&handle.id).await?.power.is_running() {
            self.api.start(&handle.id).await?;
        }
        self.api
            .connect(
                &handle.id,
                &ManagedConnectRequest {
                    local_socket: self.local_socket.clone(),
                    remote_socket: self.remote_socket.clone(),
                    ports: ports.to_vec(),
                },
            )
            .await
    }

    async fn destroy(&self, handle: &BoxHandle) -> Result<(), SandboxError> {
        self.validate(handle)?;
        self.api.destroy(&handle.id).await
    }
}

/// Observable fake-Kubernetes state for conductor and provider tests.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManagedSandboxSnapshot {
    pub id: String,
    pub name: String,
    pub namespace: String,
    pub pod: String,
    pub replicas: u8,
    pub region: String,
    pub project: String,
}

#[derive(Debug, Default)]
struct InMemoryState {
    next_id: u64,
    records: HashMap<String, ManagedSandboxSnapshot>,
}

/// Deterministic in-memory managed control plane.
///
/// It models namespace/pod allocation and scale-to-zero semantics without a
/// Kubernetes cluster, making the third provider independently testable.
#[derive(Debug, Default)]
pub struct InMemoryManagedApi {
    state: Mutex<InMemoryState>,
}

impl InMemoryManagedApi {
    /// Create an empty in-memory control plane.
    pub fn new() -> Self {
        Self::default()
    }

    /// Inspect one managed instance without exposing the fake's lock.
    pub fn snapshot(&self, id: &str) -> Result<Option<ManagedSandboxSnapshot>, SandboxError> {
        Ok(self.lock_state()?.records.get(id).cloned())
    }

    fn lock_state(&self) -> Result<MutexGuard<'_, InMemoryState>, SandboxError> {
        self.state.lock().map_err(|error| SandboxError::Task {
            operation: "lock in-memory managed control plane",
            detail: error.to_string(),
        })
    }

    fn set_replicas(&self, id: &str, replicas: u8) -> Result<(), SandboxError> {
        let mut state = self.lock_state()?;
        let record = state.records.get_mut(id).ok_or_else(|| {
            SandboxError::Config(format!("managed sandbox {id:?} does not exist"))
        })?;
        record.replicas = replicas;
        Ok(())
    }
}

impl ManagedSandboxApi for InMemoryManagedApi {
    fn ensure<'a>(&'a self, spec: &'a SandboxSpec) -> ManagedApiFuture<'a, ManagedInstance> {
        Box::pin(async move {
            let mut state = self.lock_state()?;
            if let Some(existing) = state.records.values_mut().find(|row| row.name == spec.name) {
                existing.replicas = 1;
                return Ok(instance(existing));
            }

            state.next_id += 1;
            let id = format!("managed-{}", state.next_id);
            let namespace = kubernetes_name(&spec.name);
            let record = ManagedSandboxSnapshot {
                id: id.clone(),
                name: spec.name.clone(),
                namespace: namespace.clone(),
                pod: format!("{namespace}-agent-0"),
                replicas: 1,
                region: spec.region.clone(),
                project: spec.project.clone(),
            };
            let result = instance(&record);
            state.records.insert(id, record);
            Ok(result)
        })
    }

    fn start<'a>(&'a self, id: &'a str) -> ManagedApiFuture<'a, ()> {
        Box::pin(async move { self.set_replicas(id, 1) })
    }

    fn stop<'a>(&'a self, id: &'a str) -> ManagedApiFuture<'a, ()> {
        Box::pin(async move { self.set_replicas(id, 0) })
    }

    fn status<'a>(&'a self, id: &'a str) -> ManagedApiFuture<'a, BoxStatus> {
        Box::pin(async move {
            let state = self.lock_state()?;
            let record = state.records.get(id).ok_or_else(|| {
                SandboxError::Config(format!("managed sandbox {id:?} does not exist"))
            })?;
            let running = record.replicas > 0;
            Ok(BoxStatus {
                power: if running {
                    PowerState::Running
                } else {
                    PowerState::Stopped
                },
                reachable: running,
            })
        })
    }

    fn connect<'a>(
        &'a self,
        id: &'a str,
        request: &'a ManagedConnectRequest,
    ) -> ManagedApiFuture<'a, Tunnel> {
        Box::pin(async move {
            if !self.status(id).await?.power.is_running() {
                return Err(SandboxError::Config(format!(
                    "managed sandbox {id:?} is scaled to zero"
                )));
            }
            let mut args = vec![
                "connect".to_string(),
                "--sandbox".to_string(),
                id.to_string(),
                "--local-socket".to_string(),
                request.local_socket.display().to_string(),
                "--remote-socket".to_string(),
                request.remote_socket.clone(),
            ];
            for port in &request.ports {
                args.extend(["--port".to_string(), format!("127.0.0.1:{port}")]);
            }
            Ok(Tunnel {
                // This executable belongs to the fake contract only. A real
                // backend returns its authenticated ingress/exec command.
                program: "lazybox-managed-test-tunnel".to_string(),
                args,
                env: Vec::new(),
                local_socket: request.local_socket.clone(),
                ports: request.ports.clone(),
            })
        })
    }

    fn destroy<'a>(&'a self, id: &'a str) -> ManagedApiFuture<'a, ()> {
        Box::pin(async move {
            let removed = self.lock_state()?.records.remove(id);
            if removed.is_none() {
                return Err(SandboxError::Config(format!(
                    "managed sandbox {id:?} does not exist"
                )));
            }
            Ok(())
        })
    }
}

fn instance(record: &ManagedSandboxSnapshot) -> ManagedInstance {
    ManagedInstance {
        id: record.id.clone(),
        region: record.region.clone(),
        project: record.project.clone(),
        power_state: if record.replicas > 0 {
            PowerState::Running
        } else {
            PowerState::Stopped
        },
    }
}

fn kubernetes_name(name: &str) -> String {
    let normalized = name
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || character == '-' {
                character.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect::<String>();
    let normalized = normalized.trim_matches('-');
    let suffix = if normalized.is_empty() {
        "sandbox"
    } else {
        normalized
    };
    format!("lazybox-{suffix}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Deployment;

    fn spec(name: &str) -> SandboxSpec {
        SandboxSpec {
            provider: PROVIDER_ID.to_string(),
            name: name.to_string(),
            project: "tenant-a".to_string(),
            region: "us-central1".to_string(),
            zone: String::new(),
            deployment: Deployment::default_recipe().unwrap(),
            install_lazybox: true,
            lazybox_git_sha: "abc123".to_string(),
        }
    }

    fn provider(api: Arc<InMemoryManagedApi>) -> ManagedProvider {
        ManagedProvider::new(
            api,
            "/home/lazybox/.lazybox/run/daemon.sock".to_string(),
            PathBuf::from("/tmp/lazybox-managed.sock"),
        )
    }

    #[tokio::test]
    async fn lifecycle_creates_scales_connects_and_destroys_kubernetes_resources() {
        let api = Arc::new(InMemoryManagedApi::new());
        let provider = provider(Arc::clone(&api));
        provider.check_auth().await.unwrap();
        assert_eq!(provider.id(), PROVIDER_ID);

        let handle = provider.ensure(&spec("Team One/PR 42")).await.unwrap();
        let created = api.snapshot(&handle.id).unwrap().unwrap();
        assert_eq!(created.namespace, "lazybox-team-one-pr-42");
        assert_eq!(created.pod, "lazybox-team-one-pr-42-agent-0");
        assert_eq!(created.replicas, 1);

        provider.stop(&handle).await.unwrap();
        assert_eq!(
            provider.status(&handle).await.unwrap(),
            BoxStatus {
                power: PowerState::Stopped,
                reachable: false,
            }
        );

        let tunnel = provider.connect(&handle, &[3000, 8082]).await.unwrap();
        assert_eq!(tunnel.program, "lazybox-managed-test-tunnel");
        assert_eq!(
            tunnel.local_socket,
            PathBuf::from("/tmp/lazybox-managed.sock")
        );
        assert_eq!(tunnel.ports, vec![3000, 8082]);
        assert!(
            tunnel
                .args
                .windows(2)
                .any(|pair| pair == ["--port", "127.0.0.1:3000"])
        );
        assert_eq!(api.snapshot(&handle.id).unwrap().unwrap().replicas, 1);

        provider.destroy(&handle).await.unwrap();
        assert_eq!(api.snapshot(&handle.id).unwrap(), None);
    }

    #[tokio::test]
    async fn ensure_is_idempotent_and_resumes_the_existing_namespace() {
        let api = Arc::new(InMemoryManagedApi::new());
        let provider = provider(Arc::clone(&api));
        let sandbox = spec("same");

        let first = provider.ensure(&sandbox).await.unwrap();
        provider.stop(&first).await.unwrap();
        let second = provider.ensure(&sandbox).await.unwrap();

        assert_eq!(first.id, second.id);
        assert_eq!(api.snapshot(&first.id).unwrap().unwrap().replicas, 1);
    }

    #[tokio::test]
    async fn provider_guards_specs_and_handles_before_calling_the_backend() {
        let api = Arc::new(InMemoryManagedApi::new());
        let provider = provider(api);
        let mut wrong_spec = spec("wrong");
        wrong_spec.provider = "gcp".to_string();
        let error = provider.ensure(&wrong_spec).await.unwrap_err().to_string();
        assert!(error.contains("belongs to provider \"gcp\""), "{error}");

        let wrong_handle = BoxHandle {
            provider: "e2b".to_string(),
            id: "foreign".to_string(),
            region: String::new(),
            zone: String::new(),
            project: String::new(),
            power_state: PowerState::Running,
            last_active: None,
        };
        let error = provider
            .status(&wrong_handle)
            .await
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("belongs to sandbox provider \"e2b\""),
            "{error}"
        );
    }

    #[tokio::test]
    async fn missing_instances_fail_instead_of_claiming_success() {
        let api = Arc::new(InMemoryManagedApi::new());
        let provider = provider(api);
        let handle = BoxHandle {
            provider: PROVIDER_ID.to_string(),
            id: "missing".to_string(),
            region: String::new(),
            zone: String::new(),
            project: String::new(),
            power_state: PowerState::Unknown,
            last_active: None,
        };

        for error in [
            provider.start(&handle).await.unwrap_err(),
            provider.stop(&handle).await.unwrap_err(),
            provider.status(&handle).await.unwrap_err(),
            provider.destroy(&handle).await.unwrap_err(),
        ] {
            assert!(error.to_string().contains("does not exist"));
        }
    }
}
