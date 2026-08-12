//! Native GitHub OAuth device flow — a `gh`-free credential path.
//!
//! Today the credential chain backstops the env vars with `gh auth token`,
//! so a machine without the `gh` CLI installed and logged in has no way to
//! authenticate. This module lets lazybox obtain its own token through
//! GitHub's [OAuth device flow]: the user runs `lazybox auth login github`,
//! visits a URL, enters a short code, and lazybox polls until GitHub hands
//! back an access token, which it persists under `<state>/oauth/github.json`.
//!
//! [`OAuthTokenProvider`] then reads that stored token so polling and
//! mutations work with no `gh` present. It sits between the env providers
//! and the `gh` fallback in [`crate::credential_chain`], so an explicit
//! `GH_TOKEN` still wins and `gh auth token` still backstops.
//!
//! The device flow needs a registered GitHub OAuth app's **client id**
//! (public, not a secret). It is read from the `LAZYBOX_GITHUB_OAUTH_CLIENT_ID`
//! environment variable, falling back to the baked-in [`BAKED_CLIENT_ID`].
//! Refresh tokens are out of scope: classic OAuth-app device tokens do not
//! expire, so there is nothing to refresh.
//!
//! [OAuth device flow]: https://docs.github.com/en/apps/oauth-apps/building-oauth-apps/authorizing-oauth-apps#device-flow

use lazybox_auth::{Credential, CredentialError, CredentialProvider};
use serde::{Deserialize, Serialize};
use std::fmt;
use std::path::PathBuf;
use std::time::Duration;

/// GitHub endpoint that issues a device + user code.
pub const DEVICE_CODE_URL: &str = "https://github.com/login/device/code";
/// GitHub endpoint polled to exchange the device code for an access token.
pub const ACCESS_TOKEN_URL: &str = "https://github.com/login/oauth/access_token";

/// Scopes requested for the token. `repo` covers reading and mutating PRs
/// and issues (comment, label, merge, update-branch); `read:org` lets the
/// scope picker enumerate org repositories, matching what a default
/// `gh auth login` token carries.
pub const DEFAULT_SCOPES: &str = "repo read:org";

/// Environment variable overriding the OAuth app client id.
pub const CLIENT_ID_ENV: &str = "LAZYBOX_GITHUB_OAUTH_CLIENT_ID";

/// Baked-in client id for lazybox's registered GitHub OAuth app. Empty
/// until the app is registered upstream; the env override always wins, so
/// a self-hoster can point lazybox at their own app without a rebuild.
const BAKED_CLIENT_ID: &str = "";

/// The OAuth app client id to use, or `None` when neither the env override
/// nor a baked-in value is set (in which case login cannot proceed).
pub fn client_id() -> Option<String> {
    if let Ok(v) = std::env::var(CLIENT_ID_ENV)
        && !v.is_empty()
    {
        return Some(v);
    }
    if BAKED_CLIENT_ID.is_empty() {
        None
    } else {
        Some(BAKED_CLIENT_ID.to_string())
    }
}

/// Where the resolved OAuth token is persisted: `<state>/oauth/github.json`.
pub fn token_path() -> PathBuf {
    lazybox_core::paths::state_root()
        .join("oauth")
        .join("github.json")
}

/// A persisted GitHub access token plus the metadata needed to describe it.
/// The token itself is redacted from `Debug` so it never lands in logs.
#[derive(Clone, Serialize, Deserialize)]
pub struct StoredToken {
    pub access_token: String,
    #[serde(default)]
    pub token_type: String,
    #[serde(default)]
    pub scope: String,
    /// RFC 3339 timestamp of when the token was obtained (informational).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub obtained_at: Option<String>,
}

impl fmt::Debug for StoredToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("StoredToken")
            .field("access_token", &"[REDACTED]")
            .field("token_type", &self.token_type)
            .field("scope", &self.scope)
            .field("obtained_at", &self.obtained_at)
            .finish()
    }
}

/// Persist `token` to [`token_path`], creating the parent directory. The
/// file holds a secret, so it is written `0600` on unix.
pub fn save_token(token: &StoredToken) -> std::io::Result<()> {
    let path = token_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let json = serde_json::to_string_pretty(token)?;
    std::fs::write(&path, json)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
}

/// Load the persisted token, or `None` when the file is absent, unreadable,
/// or malformed. A corrupt file is treated as "not logged in" rather than a
/// hard error so the credential chain can fall through to `gh`.
pub fn load_token() -> Option<StoredToken> {
    let bytes = std::fs::read(token_path()).ok()?;
    serde_json::from_slice(&bytes).ok()
}

/// Remove the persisted token. Missing file is success (idempotent logout).
pub fn delete_token() -> std::io::Result<()> {
    match std::fs::remove_file(token_path()) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
}

/// Credential provider that reads the token persisted by the device flow.
/// Declines with [`CredentialError::NotFound`] when no token is stored, so
/// the chain treats it as mere absence and continues to `gh auth token`.
pub struct OAuthTokenProvider;

impl CredentialProvider for OAuthTokenProvider {
    fn name(&self) -> &str {
        "github-oauth"
    }

    async fn resolve(&self, _scope: &str) -> Result<Credential, CredentialError> {
        match load_token() {
            Some(t) if !t.access_token.is_empty() => {
                Ok(Credential::new(t.access_token, "oauth:github"))
            }
            _ => Err(CredentialError::NotFound(
                "no stored GitHub OAuth token".into(),
            )),
        }
    }
}

/// The first stage of the device flow: GitHub's response to a device-code
/// request. The user visits `verification_uri` and enters `user_code`.
#[derive(Debug, Clone, Deserialize)]
pub struct DeviceCodeResponse {
    pub device_code: String,
    pub user_code: String,
    pub verification_uri: String,
    pub expires_in: u64,
    pub interval: u64,
}

/// The classified result of one access-token poll. Kept separate from the
/// network layer so the poll-response decoding is a pure, tested function.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PollOutcome {
    Authorized {
        access_token: String,
        token_type: String,
        scope: String,
    },
    /// The user has not authorized yet — keep polling at the same interval.
    Pending,
    /// GitHub asked us to back off — add 5s to the interval and keep polling.
    SlowDown,
    /// The user explicitly denied the request.
    Denied,
    /// The device code expired before authorization.
    Expired,
    /// Any other error, carrying GitHub's description.
    Error(String),
}

#[derive(Debug, thiserror::Error)]
pub enum DeviceFlowError {
    #[error("network error talking to GitHub: {0}")]
    Http(String),
    #[error("could not parse GitHub's response: {0}")]
    Parse(String),
    #[error("authorization was denied")]
    Denied,
    #[error("the device code expired before authorization completed")]
    Expired,
    #[error("GitHub rejected the request: {0}")]
    Provider(String),
}

/// Decode GitHub's device-code JSON response.
pub fn parse_device_code_response(body: &str) -> Result<DeviceCodeResponse, DeviceFlowError> {
    if let Ok(dc) = serde_json::from_str::<DeviceCodeResponse>(body) {
        return Ok(dc);
    }
    // GitHub reports a bad client id / config as a JSON error object.
    #[derive(Deserialize)]
    struct ErrBody {
        error: Option<String>,
        error_description: Option<String>,
    }
    if let Ok(e) = serde_json::from_str::<ErrBody>(body)
        && let Some(err) = e.error
    {
        return Err(DeviceFlowError::Provider(
            e.error_description.unwrap_or(err),
        ));
    }
    Err(DeviceFlowError::Parse(format!(
        "unexpected device-code response: {body}"
    )))
}

/// Classify one access-token poll body into a [`PollOutcome`]. Pure so the
/// state machine (pending → slow_down → authorized/denied/expired) is
/// tested without a network.
pub fn parse_poll_body(body: &str) -> PollOutcome {
    #[derive(Deserialize)]
    struct Resp {
        access_token: Option<String>,
        token_type: Option<String>,
        scope: Option<String>,
        error: Option<String>,
        error_description: Option<String>,
    }
    let resp = match serde_json::from_str::<Resp>(body) {
        Ok(r) => r,
        Err(e) => return PollOutcome::Error(format!("malformed token response: {e}")),
    };
    if let Some(token) = resp.access_token {
        return PollOutcome::Authorized {
            access_token: token,
            token_type: resp.token_type.unwrap_or_default(),
            scope: resp.scope.unwrap_or_default(),
        };
    }
    match resp.error.as_deref() {
        Some("authorization_pending") => PollOutcome::Pending,
        Some("slow_down") => PollOutcome::SlowDown,
        Some("access_denied") => PollOutcome::Denied,
        Some("expired_token") => PollOutcome::Expired,
        Some(other) => {
            PollOutcome::Error(resp.error_description.unwrap_or_else(|| other.to_string()))
        }
        None => PollOutcome::Error("token response had neither access_token nor error".into()),
    }
}

/// Percent-encode a form value, escaping everything outside the
/// `application/x-www-form-urlencoded` unreserved set. Keeps the request
/// body dependency-free (reqwest's `.form()` helper is feature-gated out by
/// `default-features = false`).
fn urlencode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for &b in s.as_bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

async fn post_form(url: &str, params: &[(&str, &str)]) -> Result<String, DeviceFlowError> {
    let body = params
        .iter()
        .map(|(k, v)| format!("{}={}", urlencode(k), urlencode(v)))
        .collect::<Vec<_>>()
        .join("&");
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(15))
        .build()
        .map_err(|e| DeviceFlowError::Http(e.to_string()))?;
    let resp = client
        .post(url)
        .header(reqwest::header::ACCEPT, "application/json")
        .header(
            reqwest::header::CONTENT_TYPE,
            "application/x-www-form-urlencoded",
        )
        .body(body)
        .send()
        .await
        .map_err(|e| DeviceFlowError::Http(e.to_string()))?;
    resp.text()
        .await
        .map_err(|e| DeviceFlowError::Http(e.to_string()))
}

/// Request a device + user code from GitHub (device-flow stage 1).
pub async fn request_device_code(
    client_id: &str,
    scopes: &str,
) -> Result<DeviceCodeResponse, DeviceFlowError> {
    let body = post_form(
        DEVICE_CODE_URL,
        &[("client_id", client_id), ("scope", scopes)],
    )
    .await?;
    parse_device_code_response(&body)
}

/// Poll GitHub once for the access token (device-flow stage 2).
pub async fn poll_once(client_id: &str, device_code: &str) -> Result<PollOutcome, DeviceFlowError> {
    let body = post_form(
        ACCESS_TOKEN_URL,
        &[
            ("client_id", client_id),
            ("device_code", device_code),
            ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
        ],
    )
    .await?;
    Ok(parse_poll_body(&body))
}

/// Poll until the user authorizes, respecting GitHub's interval and
/// `slow_down` backoff and giving up when the device code expires.
pub async fn poll_for_token(
    client_id: &str,
    dc: &DeviceCodeResponse,
) -> Result<StoredToken, DeviceFlowError> {
    let mut interval = Duration::from_secs(dc.interval.max(1));
    let start = tokio::time::Instant::now();
    let ttl = Duration::from_secs(dc.expires_in);
    loop {
        tokio::time::sleep(interval).await;
        if start.elapsed() > ttl {
            return Err(DeviceFlowError::Expired);
        }
        match poll_once(client_id, &dc.device_code).await? {
            PollOutcome::Authorized {
                access_token,
                token_type,
                scope,
            } => {
                return Ok(StoredToken {
                    access_token,
                    token_type,
                    scope,
                    obtained_at: Some(chrono::Utc::now().to_rfc3339()),
                });
            }
            PollOutcome::Pending => continue,
            PollOutcome::SlowDown => {
                interval += Duration::from_secs(5);
                continue;
            }
            PollOutcome::Denied => return Err(DeviceFlowError::Denied),
            PollOutcome::Expired => return Err(DeviceFlowError::Expired),
            PollOutcome::Error(m) => return Err(DeviceFlowError::Provider(m)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Serializes the env-var + `LAZYBOX_HOME` mutations, which are
    /// process-global — cargo runs a crate's tests in parallel threads.
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn with_home<T>(dir: &std::path::Path, f: impl FnOnce() -> T) -> T {
        let _lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let prev = std::env::var("LAZYBOX_HOME").ok();
        // Safety: guarded by ENV_LOCK for the closure's whole body.
        unsafe { std::env::set_var("LAZYBOX_HOME", dir) };
        let out = f();
        unsafe {
            match prev {
                Some(v) => std::env::set_var("LAZYBOX_HOME", v),
                None => std::env::remove_var("LAZYBOX_HOME"),
            }
        }
        out
    }

    #[test]
    fn parse_device_code_success() {
        let body = r#"{"device_code":"dc","user_code":"WXYZ-1234","verification_uri":"https://github.com/login/device","expires_in":900,"interval":5}"#;
        let dc = parse_device_code_response(body).expect("valid response");
        assert_eq!(dc.user_code, "WXYZ-1234");
        assert_eq!(dc.interval, 5);
        assert_eq!(dc.expires_in, 900);
    }

    #[test]
    fn parse_device_code_error_body_surfaces_description() {
        let body = r#"{"error":"unauthorized","error_description":"client id is bad"}"#;
        match parse_device_code_response(body) {
            Err(DeviceFlowError::Provider(m)) => assert!(m.contains("client id is bad")),
            other => panic!("expected Provider error, got {other:?}"),
        }
    }

    #[test]
    fn poll_body_classification() {
        assert_eq!(
            parse_poll_body(r#"{"error":"authorization_pending"}"#),
            PollOutcome::Pending
        );
        assert_eq!(
            parse_poll_body(r#"{"error":"slow_down","interval":10}"#),
            PollOutcome::SlowDown
        );
        assert_eq!(
            parse_poll_body(r#"{"error":"access_denied"}"#),
            PollOutcome::Denied
        );
        assert_eq!(
            parse_poll_body(r#"{"error":"expired_token"}"#),
            PollOutcome::Expired
        );
        match parse_poll_body(r#"{"error":"unsupported_grant_type","error_description":"nope"}"#) {
            PollOutcome::Error(m) => assert_eq!(m, "nope"),
            other => panic!("expected Error, got {other:?}"),
        }
    }

    #[test]
    fn poll_body_authorized() {
        let body = r#"{"access_token":"gho_secret","token_type":"bearer","scope":"repo,read:org"}"#;
        assert_eq!(
            parse_poll_body(body),
            PollOutcome::Authorized {
                access_token: "gho_secret".into(),
                token_type: "bearer".into(),
                scope: "repo,read:org".into(),
            }
        );
    }

    #[test]
    fn poll_body_malformed_is_error_not_panic() {
        match parse_poll_body("not json") {
            PollOutcome::Error(_) => {}
            other => panic!("expected Error, got {other:?}"),
        }
    }

    #[test]
    fn store_roundtrip_and_delete() {
        let dir = tempfile::tempdir().expect("tempdir");
        with_home(dir.path(), || {
            assert!(load_token().is_none(), "no token before save");
            let token = StoredToken {
                access_token: "gho_abc".into(),
                token_type: "bearer".into(),
                scope: "repo".into(),
                obtained_at: Some("2026-08-12T00:00:00Z".into()),
            };
            save_token(&token).expect("save");
            let loaded = load_token().expect("token after save");
            assert_eq!(loaded.access_token, "gho_abc");
            assert_eq!(loaded.scope, "repo");
            delete_token().expect("delete");
            assert!(load_token().is_none(), "gone after delete");
            // Idempotent: deleting a missing file is not an error.
            delete_token().expect("second delete is ok");
        });
    }

    #[test]
    fn stored_token_debug_redacts_secret() {
        let token = StoredToken {
            access_token: "gho_supersecret".into(),
            token_type: "bearer".into(),
            scope: "repo".into(),
            obtained_at: None,
        };
        let dbg = format!("{token:?}");
        assert!(!dbg.contains("gho_supersecret"), "token must be redacted");
        assert!(dbg.contains("REDACTED"));
    }

    #[tokio::test]
    async fn provider_declines_when_no_token() {
        let dir = tempfile::tempdir().expect("tempdir");
        let out = {
            let _lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
            let prev = std::env::var("LAZYBOX_HOME").ok();
            unsafe { std::env::set_var("LAZYBOX_HOME", dir.path()) };
            let out = OAuthTokenProvider.resolve("github").await;
            unsafe {
                match prev {
                    Some(v) => std::env::set_var("LAZYBOX_HOME", v),
                    None => std::env::remove_var("LAZYBOX_HOME"),
                }
            }
            out
        };
        assert!(matches!(out, Err(CredentialError::NotFound(_))));
    }

    #[tokio::test]
    async fn provider_resolves_stored_token() {
        let dir = tempfile::tempdir().expect("tempdir");
        let out = {
            let _lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
            let prev = std::env::var("LAZYBOX_HOME").ok();
            unsafe { std::env::set_var("LAZYBOX_HOME", dir.path()) };
            save_token(&StoredToken {
                access_token: "gho_live".into(),
                token_type: "bearer".into(),
                scope: "repo".into(),
                obtained_at: None,
            })
            .expect("save");
            let out = OAuthTokenProvider.resolve("github").await;
            unsafe {
                match prev {
                    Some(v) => std::env::set_var("LAZYBOX_HOME", v),
                    None => std::env::remove_var("LAZYBOX_HOME"),
                }
            }
            out
        };
        let cred = out.expect("token resolves");
        assert_eq!(cred.token(), "gho_live");
        assert_eq!(cred.source, "oauth:github");
    }

    #[test]
    fn urlencode_escapes_form_reserved_chars() {
        // Space and the grant-type colons must be percent-escaped so the
        // form body GitHub parses is well-formed.
        assert_eq!(urlencode("repo read:org"), "repo%20read%3Aorg");
        assert_eq!(
            urlencode("urn:ietf:params:oauth:grant-type:device_code"),
            "urn%3Aietf%3Aparams%3Aoauth%3Agrant-type%3Adevice_code"
        );
        // Unreserved characters pass through untouched.
        assert_eq!(urlencode("Iv1.abc-DEF_9~z"), "Iv1.abc-DEF_9~z");
    }

    #[test]
    fn client_id_prefers_env_override() {
        let _lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let prev = std::env::var(CLIENT_ID_ENV).ok();
        unsafe { std::env::set_var(CLIENT_ID_ENV, "Iv1.testclientid") };
        assert_eq!(client_id().as_deref(), Some("Iv1.testclientid"));
        unsafe {
            match prev {
                Some(v) => std::env::set_var(CLIENT_ID_ENV, v),
                None => std::env::remove_var(CLIENT_ID_ENV),
            }
        }
    }
}
