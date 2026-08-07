//! Per-worktree remote-box lifecycle, backed by the store.
//!
//! [`RemoteHostManager`] wraps the store-free [`GcloudProvisioner`] with
//! persistence: the [`RemoteHost`] handle for a worktree is written under
//! `remote-host:<worktree-key>` so reuse survives a client reconnect and
//! a later teardown finds the box even after a daemon restart.
//!
//! It stays inert unless `remote.enabled` is set with a complete config —
//! [`RemoteHostManager::from_config`] returns `None` otherwise, so the
//! local (daemon-host) worktree path is never touched.

use std::sync::Arc;

use lazybox_config::RemoteHostConfig;
use lazybox_git_ops::remote::{
    GcloudProvisioner, RemoteError, RemoteHost, RemoteHostSpec, instance_name,
};
use lazybox_store::Store;

fn kv_key(worktree_key: &str) -> String {
    format!("remote-host:{worktree_key}")
}

/// Reuse-or-provision a box on open, stop it on idle, tear it down on
/// worktree cleanup — persisting the handle against the worktree.
pub struct RemoteHostManager {
    provisioner: GcloudProvisioner,
    store: Arc<dyn Store>,
    config: RemoteHostConfig,
}

impl RemoteHostManager {
    /// Build a manager, or `None` when remote targeting is off or the
    /// config is incomplete (missing project/zone, or neither a source
    /// image nor an instance template). A `None` here is the signal for
    /// every caller to no-op.
    pub fn from_config(config: &RemoteHostConfig, store: Arc<dyn Store>) -> Option<Self> {
        if !config.enabled
            || config.project.is_empty()
            || config.zone.is_empty()
            || (config.source_image.is_none() && config.instance_template.is_none())
        {
            return None;
        }
        Some(Self {
            provisioner: GcloudProvisioner::new(),
            store,
            config: config.clone(),
        })
    }

    fn spec(&self) -> RemoteHostSpec {
        RemoteHostSpec {
            project: self.config.project.clone(),
            zone: self.config.zone.clone(),
            machine_type: self.config.machine_type.clone(),
            source_image: self.config.source_image.clone(),
            instance_template: self.config.instance_template.clone(),
        }
    }

    /// Reuse-or-provision the box for `worktree_key` and persist its
    /// handle. Idempotent: a second call reuses the same box (the
    /// instance name is derived deterministically from the key).
    pub async fn ensure(&self, worktree_key: &str) -> Result<RemoteHost, RemoteError> {
        let name = instance_name(&self.config.instance_prefix, worktree_key);
        let host = self.provisioner.ensure(&name, &self.spec()).await?;
        self.persist(worktree_key, &host);
        Ok(host)
    }

    /// Stop the worktree's box (stop-on-idle) if one is recorded. No-op
    /// otherwise.
    pub async fn stop(&self, worktree_key: &str) -> Result<(), RemoteError> {
        if let Some(host) = self.handle(worktree_key) {
            self.provisioner.stop(&host).await?;
        }
        Ok(())
    }

    /// Tear the worktree's box down and forget its handle. No-op when
    /// nothing was provisioned; safe to call twice.
    pub async fn teardown(&self, worktree_key: &str) -> Result<(), RemoteError> {
        if let Some(host) = self.handle(worktree_key) {
            self.provisioner.teardown(&host).await?;
            let _ = self.store.delete_kv(&kv_key(worktree_key));
        }
        Ok(())
    }

    /// The persisted handle for a worktree, if a box was provisioned.
    pub fn handle(&self, worktree_key: &str) -> Option<RemoteHost> {
        let raw = self.store.get_kv(&kv_key(worktree_key)).ok().flatten()?;
        serde_json::from_str(&raw).ok()
    }

    fn persist(&self, worktree_key: &str, host: &RemoteHost) {
        match serde_json::to_string(host) {
            Ok(json) => {
                if let Err(error) = self.store.set_kv(&kv_key(worktree_key), &json) {
                    tracing::warn!(%error, worktree = worktree_key, "persist remote host handle");
                }
            }
            Err(error) => tracing::warn!(%error, "serialize remote host handle"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lazybox_git_ops::remote::{GcloudFuture, GcloudRunner};
    use lazybox_store::MemoryStore;
    use std::collections::VecDeque;
    use std::os::unix::process::ExitStatusExt;
    use std::process::{ExitStatus, Output};
    use std::sync::Mutex;

    fn ok(stdout: &str) -> Output {
        Output {
            status: ExitStatus::from_raw(0),
            stdout: stdout.as_bytes().to_vec(),
            stderr: Vec::new(),
        }
    }

    struct ScriptedGcloud {
        queue: Mutex<VecDeque<Output>>,
    }

    impl GcloudRunner for ScriptedGcloud {
        fn run<'a>(&'a self, _args: &'a [&'a str]) -> GcloudFuture<'a, Output> {
            let out = self.queue.lock().unwrap().pop_front().expect("extra call");
            Box::pin(async move { Ok(out) })
        }
    }

    fn manager(store: Arc<dyn Store>, outputs: Vec<Output>) -> RemoteHostManager {
        RemoteHostManager {
            provisioner: GcloudProvisioner::with_runner(Arc::new(ScriptedGcloud {
                queue: Mutex::new(outputs.into()),
            })),
            store,
            config: RemoteHostConfig {
                enabled: true,
                project: "proj".into(),
                zone: "z".into(),
                machine_type: "e2-standard-8".into(),
                source_image: Some("golden".into()),
                instance_template: None,
                instance_prefix: "lazybox".into(),
            },
        }
    }

    fn describe(id: &str, status: &str) -> String {
        format!(r#"{{"id":"{id}","status":"{status}"}}"#)
    }

    #[test]
    fn from_config_is_none_when_disabled_or_incomplete() {
        let store: Arc<dyn Store> = Arc::new(MemoryStore::new());
        assert!(
            RemoteHostManager::from_config(&RemoteHostConfig::default(), store.clone()).is_none()
        );

        // Enabled but still missing project/zone/image.
        let enabled_only = RemoteHostConfig {
            enabled: true,
            ..RemoteHostConfig::default()
        };
        assert!(RemoteHostManager::from_config(&enabled_only, store.clone()).is_none());

        let complete = RemoteHostConfig {
            enabled: true,
            project: "p".into(),
            zone: "z".into(),
            source_image: Some("img".into()),
            ..RemoteHostConfig::default()
        };
        assert!(RemoteHostManager::from_config(&complete, store).is_some());
    }

    #[tokio::test]
    async fn ensure_persists_the_handle() {
        let store: Arc<dyn Store> = Arc::new(MemoryStore::new());
        let mgr = manager(store.clone(), vec![ok(&describe("42", "RUNNING"))]);

        let host = mgr.ensure("acme-widget").await.unwrap();
        assert_eq!(host.id, "42");

        // Persisted under the worktree key and readable back.
        let stored = mgr.handle("acme-widget").unwrap();
        assert_eq!(stored, host);
        assert_eq!(
            store.get_kv("remote-host:acme-widget").unwrap(),
            Some(serde_json::to_string(&host).unwrap())
        );
    }

    #[tokio::test]
    async fn stop_and_teardown_no_op_without_a_handle() {
        let store: Arc<dyn Store> = Arc::new(MemoryStore::new());
        // No gcloud calls should fire, so an empty queue must not panic.
        let mgr = manager(store, vec![]);
        mgr.stop("missing").await.unwrap();
        mgr.teardown("missing").await.unwrap();
    }

    #[tokio::test]
    async fn teardown_deletes_the_box_and_forgets_the_handle() {
        let store: Arc<dyn Store> = Arc::new(MemoryStore::new());
        let mgr = manager(store.clone(), vec![ok(&describe("7", "RUNNING")), ok("")]);

        mgr.ensure("wt").await.unwrap();
        assert!(mgr.handle("wt").is_some());

        mgr.teardown("wt").await.unwrap();
        assert!(mgr.handle("wt").is_none());
        assert_eq!(store.get_kv("remote-host:wt").unwrap(), None);
    }
}
