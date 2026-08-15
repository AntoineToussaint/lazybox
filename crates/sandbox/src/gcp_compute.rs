//! Compute Engine lifecycle over the REST API (#1126).
//!
//! Replaces the `gcloud compute instances start/stop/describe` shells with
//! direct calls to `compute.googleapis.com`, authenticated by a natively
//! minted ADC token ([`crate::gcp_auth`]) — no `gcloud` on PATH. The
//! provider talks to this through the [`ComputeClient`] trait so its
//! lifecycle logic (`status`, wake-on-connect) stays unit-tested against a
//! scripted fake, while the real HTTP + token glue lives in [`HttpCompute`].

use std::fmt::Debug;
use std::sync::Arc;

use serde::Deserialize;

use crate::gcp::{GcpAuth, parse_power_state};
use crate::provider::{CommandFuture, SandboxError};
use crate::{BoxHandle, PowerState};

const COMPUTE_BASE: &str = "https://compute.googleapis.com/compute/v1";
const PROVIDER: &str = "gcp";

/// The Compute Engine instance operations the box lifecycle needs. Boxed
/// futures keep it object-safe so the provider can hold `Arc<dyn …>` and
/// swap a scripted fake in under test.
pub trait ComputeClient: Send + Sync + Debug {
    /// `instances.get` → normalized power state.
    fn power<'a>(&'a self, handle: &'a BoxHandle) -> CommandFuture<'a, PowerState>;
    /// `instances.start`.
    fn start<'a>(&'a self, handle: &'a BoxHandle) -> CommandFuture<'a, ()>;
    /// `instances.stop`.
    fn stop<'a>(&'a self, handle: &'a BoxHandle) -> CommandFuture<'a, ()>;
    /// Mint a token and discard it — the auth preflight. Surfaces a stale
    /// credential as [`SandboxError::ReauthRequired`] before the first op.
    fn check_token<'a>(&'a self) -> CommandFuture<'a, ()>;
}

/// The `instances.get` URL for a handle.
pub fn instance_url(base: &str, handle: &BoxHandle) -> String {
    format!(
        "{base}/projects/{}/zones/{}/instances/{}",
        handle.project, handle.zone, handle.id
    )
}

/// The `instances.<verb>` action URL (`start` / `stop`).
pub fn instance_action_url(base: &str, handle: &BoxHandle, verb: &str) -> String {
    format!("{}/{verb}", instance_url(base, handle))
}

/// The single field of an `instances.get` response the power probe reads.
#[derive(Debug, Deserialize)]
struct InstanceStatus {
    #[serde(default)]
    status: String,
}

/// Read the normalized power state out of an `instances.get` body.
pub fn parse_instance_power(body: &str) -> Result<PowerState, SandboxError> {
    let parsed: InstanceStatus = serde_json::from_str(body).map_err(|e| SandboxError::Parse {
        what: "compute instance",
        detail: e.to_string(),
    })?;
    Ok(parse_power_state(&parsed.status))
}

/// Classify a Compute API HTTP failure. A 401/403 means the token is stale
/// or revoked — the same re-login remedy as a failed mint — so it maps to
/// [`SandboxError::ReauthRequired`] rather than a generic API error (#1126).
pub fn classify_http_error(status: u16, operation: &'static str, detail: &str) -> SandboxError {
    if status == 401 || status == 403 {
        return SandboxError::ReauthRequired {
            detail: format!("compute {operation} returned HTTP {status}: {detail}"),
        };
    }
    SandboxError::Api {
        provider: PROVIDER,
        operation,
        status,
        detail: detail.to_string(),
    }
}

/// Production [`ComputeClient`]: a `reqwest` client that mints an ADC token
/// per call and speaks the Compute REST API.
#[derive(Debug, Clone)]
pub struct HttpCompute {
    client: reqwest::Client,
    auth: GcpAuth,
    base: String,
}

impl HttpCompute {
    pub fn new(auth: GcpAuth) -> Self {
        Self {
            client: reqwest::Client::new(),
            auth,
            base: COMPUTE_BASE.to_string(),
        }
    }

    async fn token(&self) -> Result<String, SandboxError> {
        self.auth.access_token(&self.client).await
    }

    /// Issue a Compute op that returns no body we read (`start`/`stop`). The
    /// op is fire-and-issue: a 2xx means GCE accepted it. Reaching the
    /// terminal power state is observed by a later `status`, exactly as the
    /// wake-on-connect keepalive already tolerates.
    async fn issue(&self, handle: &BoxHandle, verb: &'static str) -> Result<(), SandboxError> {
        let token = self.token().await?;
        let url = instance_action_url(&self.base, handle, verb);
        let response = self
            .client
            .post(url)
            .bearer_auth(token)
            .send()
            .await
            .map_err(|e| SandboxError::ApiTransport {
                provider: PROVIDER,
                operation: "instance action",
                detail: e.to_string(),
            })?;
        let status = response.status();
        if status.is_success() {
            return Ok(());
        }
        let body = response.text().await.unwrap_or_default();
        Err(classify_http_error(
            status.as_u16(),
            "instance action",
            body.trim(),
        ))
    }
}

impl ComputeClient for HttpCompute {
    fn power<'a>(&'a self, handle: &'a BoxHandle) -> CommandFuture<'a, PowerState> {
        Box::pin(async move {
            let token = self.token().await?;
            let url = instance_url(&self.base, handle);
            let response = self
                .client
                .get(url)
                .bearer_auth(token)
                .send()
                .await
                .map_err(|e| SandboxError::ApiTransport {
                    provider: PROVIDER,
                    operation: "instance get",
                    detail: e.to_string(),
                })?;
            let status = response.status();
            let body = response
                .text()
                .await
                .map_err(|e| SandboxError::ApiTransport {
                    provider: PROVIDER,
                    operation: "instance get",
                    detail: e.to_string(),
                })?;
            if !status.is_success() {
                return Err(classify_http_error(
                    status.as_u16(),
                    "instance get",
                    body.trim(),
                ));
            }
            parse_instance_power(&body)
        })
    }

    fn start<'a>(&'a self, handle: &'a BoxHandle) -> CommandFuture<'a, ()> {
        Box::pin(self.issue(handle, "start"))
    }

    fn stop<'a>(&'a self, handle: &'a BoxHandle) -> CommandFuture<'a, ()> {
        Box::pin(self.issue(handle, "stop"))
    }

    fn check_token<'a>(&'a self) -> CommandFuture<'a, ()> {
        Box::pin(async move { self.token().await.map(|_| ()) })
    }
}

/// A default [`ComputeClient`] for provider construction sites that don't
/// wire one explicitly (e.g. tests building a bare provider).
pub fn default_compute(auth: GcpAuth) -> Arc<dyn ComputeClient> {
    Arc::new(HttpCompute::new(auth))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::PowerState;

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
    fn instance_url_addresses_project_zone_instance() {
        assert_eq!(
            instance_url(COMPUTE_BASE, &handle()),
            "https://compute.googleapis.com/compute/v1/projects/proj/zones/us-central1-a/instances/lazybox-sbx-abc"
        );
    }

    #[test]
    fn action_url_appends_the_verb() {
        let url = instance_action_url(COMPUTE_BASE, &handle(), "start");
        assert!(url.ends_with("/instances/lazybox-sbx-abc/start"), "{url}");
    }

    #[test]
    fn parses_the_instance_status_field() {
        let body = r#"{"status":"RUNNING","name":"lazybox-sbx-abc","kind":"compute#instance"}"#;
        assert_eq!(parse_instance_power(body).unwrap(), PowerState::Running);
    }

    #[test]
    fn a_missing_status_reads_as_unknown_not_a_panic() {
        // GCE always returns `status`, but a body without it must degrade to
        // Unknown (never silently "stopped") rather than fail to parse.
        assert_eq!(
            parse_instance_power(r#"{"name":"x"}"#).unwrap(),
            PowerState::Unknown
        );
    }

    #[test]
    fn a_non_object_body_is_a_parse_error() {
        assert!(matches!(
            parse_instance_power("not json").unwrap_err(),
            SandboxError::Parse { .. }
        ));
    }

    #[test]
    fn unauthorized_maps_to_reauth_required() {
        // A 401/403 from Compute is a stale/revoked token — actionable as a
        // re-login, not a generic API error.
        assert!(matches!(
            classify_http_error(401, "instance get", "Invalid Credentials"),
            SandboxError::ReauthRequired { .. }
        ));
        assert!(matches!(
            classify_http_error(403, "instance action", "forbidden"),
            SandboxError::ReauthRequired { .. }
        ));
    }

    #[test]
    fn other_http_errors_stay_api_errors() {
        match classify_http_error(404, "instance get", "not found") {
            SandboxError::Api {
                status, operation, ..
            } => {
                assert_eq!(status, 404);
                assert_eq!(operation, "instance get");
            }
            other => panic!("expected Api, got {other:?}"),
        }
    }
}
