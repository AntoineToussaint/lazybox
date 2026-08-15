//! Compute Engine lifecycle over the REST API (#1126).
//!
//! Replaces the `gcloud compute instances start/stop/describe` shells with
//! direct calls to `compute.googleapis.com`, authenticated by a natively
//! minted ADC token ([`crate::gcp_auth`]) — no `gcloud` on PATH. The
//! provider talks to this through the [`ComputeClient`] trait so its
//! lifecycle logic (`status`, wake-on-connect) stays unit-tested against a
//! scripted fake, while the real HTTP + token glue lives in [`HttpCompute`].

use std::fmt::Debug;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde::Deserialize;

use crate::gcp::{GcpAuth, parse_power_state};
use crate::provider::{CommandFuture, SandboxError};
use crate::{BoxHandle, PowerState};

const COMPUTE_BASE: &str = "https://compute.googleapis.com/compute/v1";
const PROVIDER: &str = "gcp";
/// Bound on every Compute REST call so a stuck endpoint fails fast rather than
/// hanging `status` (#1126).
const COMPUTE_REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
/// Assumed token lifetime when the mint endpoint reports none.
const TOKEN_CACHE_DEFAULT_TTL: Duration = Duration::from_secs(3600);
/// Refresh a cached token this far before its real expiry, so an op never
/// races the boundary and 401s on a just-expired token.
const TOKEN_REFRESH_MARGIN: Duration = Duration::from_secs(60);

/// The Compute Engine instance operations the box lifecycle needs. Boxed
/// futures keep it object-safe so the provider can hold `Arc<dyn …>` and
/// swap a scripted fake in under test.
pub trait ComputeClient: Send + Sync + Debug {
    /// `instances.get` → normalized power state.
    fn power<'a>(&'a self, handle: &'a BoxHandle) -> CommandFuture<'a, PowerState>;
    /// `instances.start`. Returns once GCE *accepts* the request, not once the
    /// instance reaches `RUNNING` — the REST call does not block on the
    /// operation the way `gcloud instances start` did. Callers that need the
    /// box actually up must poll [`power`](Self::power) / `status`
    /// afterwards (as `connect`'s keepalive and `wait_until_reachable` do).
    fn start<'a>(&'a self, handle: &'a BoxHandle) -> CommandFuture<'a, ()>;
    /// `instances.stop`. Non-blocking in the same sense as [`start`](Self::start).
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

/// Classify a Compute API HTTP failure. Only **401** (invalid/expired token)
/// carries the re-login remedy, so only it maps to
/// [`SandboxError::ReauthRequired`]. A **403** is an IAM *permission* denial
/// on an otherwise-valid identity — re-authenticating changes nothing, so it
/// must stay a plain API error naming the denial, not a reauth prompt (#1126
/// review).
pub fn classify_http_error(status: u16, operation: &'static str, detail: &str) -> SandboxError {
    if status == 401 {
        return SandboxError::ReauthRequired {
            detail: format!("compute {operation} returned HTTP 401: {detail}"),
        };
    }
    SandboxError::Api {
        provider: PROVIDER,
        operation,
        status,
        detail: detail.to_string(),
    }
}

/// A cached access token and the instant it should be refreshed by (real
/// expiry minus [`TOKEN_REFRESH_MARGIN`]).
#[derive(Debug, Clone)]
struct CachedToken {
    value: String,
    refresh_at: Instant,
}

/// Production [`ComputeClient`]: a `reqwest` client that mints an ADC token
/// (cached until near expiry) and speaks the Compute REST API.
#[derive(Debug, Clone)]
pub struct HttpCompute {
    client: reqwest::Client,
    auth: GcpAuth,
    base: String,
    /// Last minted token, reused across ops until it nears expiry so a wake
    /// poll-loop doesn't mint a fresh token on every `status` (#1126 review).
    cache: Arc<Mutex<Option<CachedToken>>>,
}

impl HttpCompute {
    pub fn new(auth: GcpAuth) -> Self {
        Self {
            client: reqwest::Client::new(),
            auth,
            base: COMPUTE_BASE.to_string(),
            cache: Arc::new(Mutex::new(None)),
        }
    }

    /// A cached token that is still comfortably valid, if any. A poisoned lock
    /// simply misses the cache (and re-mints) rather than propagating a panic.
    fn cached(&self) -> Option<String> {
        let guard = self.cache.lock().ok()?;
        let cached = guard.as_ref()?;
        (cached.refresh_at > Instant::now()).then(|| cached.value.clone())
    }

    async fn token(&self) -> Result<String, SandboxError> {
        if let Some(token) = self.cached() {
            return Ok(token);
        }
        let minted = self.auth.access_token(&self.client).await?;
        let ttl = minted.ttl.unwrap_or(TOKEN_CACHE_DEFAULT_TTL);
        let refresh_at = Instant::now() + ttl.saturating_sub(TOKEN_REFRESH_MARGIN);
        if let Ok(mut guard) = self.cache.lock() {
            *guard = Some(CachedToken {
                value: minted.value.clone(),
                refresh_at,
            });
        }
        Ok(minted.value)
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
            .timeout(COMPUTE_REQUEST_TIMEOUT)
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
                .timeout(COMPUTE_REQUEST_TIMEOUT)
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

/// The production [`ComputeClient`] — an [`HttpCompute`] over `auth` — boxed
/// for the provider's `compute` field. The construction site in `tui-boot`.
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
        // A 401 from Compute is an invalid/expired token — actionable as a
        // re-login.
        assert!(matches!(
            classify_http_error(401, "instance get", "Invalid Credentials"),
            SandboxError::ReauthRequired { .. }
        ));
    }

    #[test]
    fn forbidden_is_a_permission_error_not_a_reauth_prompt() {
        // A 403 is an IAM permission denial on a valid identity; re-auth won't
        // fix it, so it must stay an Api error naming the denial (#1126 review).
        match classify_http_error(403, "instance action", "compute.instances.start denied") {
            SandboxError::Api { status, detail, .. } => {
                assert_eq!(status, 403);
                assert!(detail.contains("denied"), "{detail}");
            }
            other => panic!("expected Api, got {other:?}"),
        }
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

    #[test]
    fn a_cached_token_is_reused_until_its_refresh_deadline() {
        // Guards #1126's fix: without caching, every op (and every 5s wake
        // poll) mints a fresh token. `cached()` must return a live entry and
        // reject an expired one so `token()` only re-mints past the deadline.
        let hc = HttpCompute::new(GcpAuth::default());
        *hc.cache.lock().expect("cache lock") = Some(CachedToken {
            value: "ya29.cached".into(),
            refresh_at: Instant::now() + Duration::from_secs(600),
        });
        assert_eq!(hc.cached().as_deref(), Some("ya29.cached"));

        *hc.cache.lock().expect("cache lock") = Some(CachedToken {
            value: "ya29.stale".into(),
            refresh_at: Instant::now() - Duration::from_secs(1),
        });
        assert_eq!(
            hc.cached(),
            None,
            "an expired cache entry must not be reused"
        );
    }
}
