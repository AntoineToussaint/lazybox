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
//! mutations work with no `gh` present. It is the **last** provider in
//! [`crate::credential_chain`], after `gh auth token`: a manually-stored
//! token can be invalidated server-side without `is_expired()` noticing, so
//! ahead of `gh` it would shadow a working `gh` credential; last, it only
//! activates when nothing better resolves — the `gh`-absent case it is for.
//!
//! The device flow needs a registered GitHub OAuth app's **client id**
//! (public, not a secret). It is read from the `LAZYBOX_GITHUB_OAUTH_CLIENT_ID`
//! environment variable, falling back to the baked-in `BAKED_CLIENT_ID`.
//!
//! Classic OAuth-app device tokens do not expire, but a **GitHub App** issues
//! an 8-hour token plus a refresh token through the same flow. We capture the
//! `expires_in` GitHub returns and treat an expired stored token as absent, so
//! a mis-registered GitHub App fails loud (re-login prompt) instead of serving
//! a dead token silently. Automatic refresh-token rotation is not implemented.
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

/// `Credential::source` label for a token resolved from the stored OAuth
/// login. Callers (e.g. setup detection) match on it to tell an invalid
/// OAuth token apart from an invalid `gh` token when advising a fix.
pub const CREDENTIAL_SOURCE: &str = "oauth:github";

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
    /// RFC 3339 expiry, set only when GitHub returns `expires_in` (a GitHub
    /// App token). Absent for classic OAuth-app tokens, which never expire.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<String>,
}

impl StoredToken {
    /// True when the token carries an expiry that has already passed. An
    /// unparseable timestamp is treated as *not* expired so a serialization
    /// quirk never discards an otherwise-usable token.
    pub fn is_expired(&self) -> bool {
        let Some(raw) = &self.expires_at else {
            return false;
        };
        match chrono::DateTime::parse_from_rfc3339(raw) {
            Ok(at) => chrono::Utc::now() >= at,
            Err(_) => false,
        }
    }
}

impl fmt::Debug for StoredToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("StoredToken")
            .field("access_token", &"[REDACTED]")
            .field("token_type", &self.token_type)
            .field("scope", &self.scope)
            .field("obtained_at", &self.obtained_at)
            .field("expires_at", &self.expires_at)
            .finish()
    }
}

/// Persist `token` to [`token_path`], creating the parent directory. The file
/// holds a secret, so on unix it is created `0600` from the start via
/// `OpenOptions` — never written at the umask default and tightened after,
/// which would leave the token group/world-readable during the write.
pub fn save_token(token: &StoredToken) -> std::io::Result<()> {
    let path = token_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let json = serde_json::to_string_pretty(token)?;

    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(&path)?;
    // An existing loose-permission file (e.g. written by an older build)
    // keeps its old mode through `open`; tighten it so a re-login repairs it.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
    }
    std::io::Write::write_all(&mut file, json.as_bytes())
}

/// Load the persisted token, or `None` when the file is absent or malformed.
/// A corrupt file is treated as "not logged in" so the chain resolves as if
/// no OAuth login exists — but it is logged, because on a `gh`-less box that
/// silent `None` is the difference between "authenticated" and a baffling
/// auth failure with a token file sitting right there.
pub fn load_token() -> Option<StoredToken> {
    let bytes = std::fs::read(token_path()).ok()?;
    match serde_json::from_slice(&bytes) {
        Ok(token) => Some(token),
        Err(e) => {
            tracing::warn!(
                path = %token_path().display(),
                error = %e,
                "stored GitHub OAuth token is unreadable; treating as logged out"
            );
            None
        }
    }
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
/// Declines with [`CredentialError::NotFound`] when no token is stored or the
/// stored one has expired, so the chain treats it as mere absence rather than
/// a hard failure. It is the chain's last resort (see [`crate::credential_chain`]).
pub struct OAuthTokenProvider;

impl CredentialProvider for OAuthTokenProvider {
    fn name(&self) -> &str {
        "github-oauth"
    }

    async fn resolve(&self, _scope: &str) -> Result<Credential, CredentialError> {
        match load_token() {
            Some(t) if t.access_token.is_empty() => Err(CredentialError::NotFound(
                "no stored GitHub OAuth token".into(),
            )),
            Some(t) if t.is_expired() => {
                tracing::warn!("stored GitHub OAuth token has expired; run `lazybox auth login`");
                Err(CredentialError::NotFound(
                    "stored GitHub OAuth token expired".into(),
                ))
            }
            Some(t) => Ok(Credential::new(t.access_token, CREDENTIAL_SOURCE)),
            None => Err(CredentialError::NotFound(
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
        /// Seconds until the token expires, present only for GitHub App
        /// tokens; `None` for non-expiring classic OAuth-app tokens.
        expires_in: Option<u64>,
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
        expires_in: Option<u64>,
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
            expires_in: resp.expires_in,
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

/// Build the HTTP client used for the device flow. Reused across the poll
/// loop (see [`poll_for_token`]) rather than rebuilt per request, which would
/// discard the connection pool and re-run TLS setup every ~5s.
fn build_http_client() -> Result<reqwest::Client, DeviceFlowError> {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(15))
        .build()
        .map_err(|e| DeviceFlowError::Http(e.to_string()))
}

async fn post_form(
    client: &reqwest::Client,
    url: &str,
    params: &[(&str, &str)],
) -> Result<String, DeviceFlowError> {
    let body = params
        .iter()
        .map(|(k, v)| format!("{}={}", urlencode(k), urlencode(v)))
        .collect::<Vec<_>>()
        .join("&");
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
    // A 5xx is a transient server-side blip, not a contract error — surface it
    // as `Http` so the poll loop retries instead of aborting the login on a
    // GitHub hiccup mid-flow. GitHub reports device-flow states (pending,
    // slow_down, bad client) as 2xx/4xx JSON bodies, which fall through.
    let status = resp.status();
    let text = resp
        .text()
        .await
        .map_err(|e| DeviceFlowError::Http(e.to_string()))?;
    if status.is_server_error() {
        return Err(DeviceFlowError::Http(format!("GitHub returned {status}")));
    }
    Ok(text)
}

/// Request a device + user code from GitHub (device-flow stage 1).
pub async fn request_device_code(
    client_id: &str,
    scopes: &str,
) -> Result<DeviceCodeResponse, DeviceFlowError> {
    let client = build_http_client()?;
    let body = post_form(
        &client,
        DEVICE_CODE_URL,
        &[("client_id", client_id), ("scope", scopes)],
    )
    .await?;
    parse_device_code_response(&body)
}

async fn poll_once_with(
    client: &reqwest::Client,
    client_id: &str,
    device_code: &str,
) -> Result<PollOutcome, DeviceFlowError> {
    let body = post_form(
        client,
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

/// Poll GitHub once for the access token (device-flow stage 2).
pub async fn poll_once(client_id: &str, device_code: &str) -> Result<PollOutcome, DeviceFlowError> {
    let client = build_http_client()?;
    poll_once_with(&client, client_id, device_code).await
}

/// Whether a poll error is transient — a network blip or a GitHub 5xx that
/// should be retried until the device code expires — rather than a terminal
/// misconfiguration (bad client id, unsupported grant) that will never
/// succeed and must abort the login now.
fn poll_error_is_transient(e: &DeviceFlowError) -> bool {
    matches!(e, DeviceFlowError::Http(_))
}

/// Build the stored token from an authorized poll, stamping `expires_at` when
/// GitHub returned an `expires_in` (a GitHub App token).
fn authorized_token(
    access_token: String,
    token_type: String,
    scope: String,
    expires_in: Option<u64>,
) -> StoredToken {
    let now = chrono::Utc::now();
    let expires_at = expires_in
        .and_then(|secs| i64::try_from(secs).ok())
        .and_then(|secs| now.checked_add_signed(chrono::Duration::seconds(secs)))
        .map(|at| at.to_rfc3339());
    StoredToken {
        access_token,
        token_type,
        scope,
        obtained_at: Some(now.to_rfc3339()),
        expires_at,
    }
}

/// Poll until the user authorizes, respecting GitHub's interval and
/// `slow_down` backoff and giving up when the device code expires. A
/// transient network error or GitHub 5xx during the flow is retried (bounded
/// by the code's TTL), not treated as a fatal login failure.
pub async fn poll_for_token(
    client_id: &str,
    dc: &DeviceCodeResponse,
) -> Result<StoredToken, DeviceFlowError> {
    let client = build_http_client()?;
    let mut interval = Duration::from_secs(dc.interval.max(1));
    let start = tokio::time::Instant::now();
    let ttl = Duration::from_secs(dc.expires_in);
    loop {
        tokio::time::sleep(interval).await;
        if start.elapsed() > ttl {
            return Err(DeviceFlowError::Expired);
        }
        let outcome = match poll_once_with(&client, client_id, &dc.device_code).await {
            Ok(outcome) => outcome,
            Err(e) if poll_error_is_transient(&e) => {
                tracing::debug!(error = %e, "transient error polling for token; retrying");
                continue;
            }
            Err(e) => return Err(e),
        };
        match outcome {
            PollOutcome::Authorized {
                access_token,
                token_type,
                scope,
                expires_in,
            } => {
                return Ok(authorized_token(
                    access_token,
                    token_type,
                    scope,
                    expires_in,
                ));
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
                expires_in: None,
            }
        );
    }

    #[test]
    fn poll_body_authorized_captures_github_app_expiry() {
        // A GitHub App token carries `expires_in`; it must be captured so the
        // stored token can be aged out instead of served dead forever.
        let body =
            r#"{"access_token":"ghs_secret","token_type":"bearer","scope":"","expires_in":28800}"#;
        match parse_poll_body(body) {
            PollOutcome::Authorized { expires_in, .. } => assert_eq!(expires_in, Some(28800)),
            other => panic!("expected Authorized, got {other:?}"),
        }
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
                expires_at: None,
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
            expires_at: None,
        };
        let dbg = format!("{token:?}");
        assert!(!dbg.contains("gho_supersecret"), "token must be redacted");
        assert!(dbg.contains("REDACTED"));
    }

    /// Resolve the provider synchronously under the env lock. A plain
    /// `#[tokio::test]` holding `ENV_LOCK` across the `.await` trips
    /// `clippy::await_holding_lock`; a current-thread `block_on` keeps the
    /// env-var mutation serialized without an await in scope.
    fn resolve_under_home(dir: &std::path::Path) -> Result<Credential, CredentialError> {
        with_home(dir, || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .build()
                .expect("test runtime");
            rt.block_on(OAuthTokenProvider.resolve("github"))
        })
    }

    #[test]
    fn provider_declines_when_no_token() {
        let dir = tempfile::tempdir().expect("tempdir");
        assert!(matches!(
            resolve_under_home(dir.path()),
            Err(CredentialError::NotFound(_))
        ));
    }

    #[test]
    fn provider_resolves_stored_token() {
        let dir = tempfile::tempdir().expect("tempdir");
        with_home(dir.path(), || {
            save_token(&StoredToken {
                access_token: "gho_live".into(),
                token_type: "bearer".into(),
                scope: "repo".into(),
                obtained_at: None,
                expires_at: None,
            })
            .expect("save");
        });
        let cred = resolve_under_home(dir.path()).expect("token resolves");
        assert_eq!(cred.token(), "gho_live");
        assert_eq!(cred.source, "oauth:github");
    }

    #[test]
    fn provider_declines_expired_token() {
        // A GitHub App token past its expiry must be treated as absent so the
        // chain re-falls-back (or prompts re-login) instead of serving a dead
        // token that 401s every request.
        let dir = tempfile::tempdir().expect("tempdir");
        with_home(dir.path(), || {
            save_token(&StoredToken {
                access_token: "ghs_dead".into(),
                token_type: "bearer".into(),
                scope: "repo".into(),
                obtained_at: None,
                expires_at: Some("2000-01-01T00:00:00Z".into()),
            })
            .expect("save");
        });
        assert!(matches!(
            resolve_under_home(dir.path()),
            Err(CredentialError::NotFound(_))
        ));
    }

    #[test]
    fn is_expired_reads_the_stamp() {
        let mut token = StoredToken {
            access_token: "t".into(),
            token_type: "bearer".into(),
            scope: String::new(),
            obtained_at: None,
            expires_at: None,
        };
        assert!(!token.is_expired(), "no expiry ⇒ never expired");
        token.expires_at = Some("2000-01-01T00:00:00Z".into());
        assert!(token.is_expired(), "past expiry ⇒ expired");
        token.expires_at = Some("2999-01-01T00:00:00Z".into());
        assert!(!token.is_expired(), "future expiry ⇒ live");
        token.expires_at = Some("not-a-timestamp".into());
        assert!(
            !token.is_expired(),
            "unparseable expiry ⇒ kept, not discarded"
        );
    }

    #[test]
    fn authorized_token_stamps_expiry_only_when_present() {
        let plain = authorized_token("t".into(), "bearer".into(), "repo".into(), None);
        assert!(plain.expires_at.is_none(), "classic OAuth token: no expiry");
        assert!(plain.obtained_at.is_some());

        let app = authorized_token("t".into(), "bearer".into(), "repo".into(), Some(28800));
        assert!(app.expires_at.is_some(), "GitHub App token: expiry stamped");
        assert!(!app.is_expired(), "a fresh 8h token is not already expired");
    }

    #[test]
    fn http_client_builds() {
        // The poll loop shares one client; a misconfigured builder would
        // fail every login, so confirm it constructs.
        assert!(build_http_client().is_ok());
    }

    #[test]
    fn transient_errors_retry_terminal_errors_abort() {
        // Network blips / 5xx are retried; a GitHub OAuth error is terminal.
        assert!(poll_error_is_transient(&DeviceFlowError::Http(
            "blip".into()
        )));
        assert!(!poll_error_is_transient(&DeviceFlowError::Provider(
            "incorrect_client_credentials".into()
        )));
        assert!(!poll_error_is_transient(&DeviceFlowError::Denied));
        assert!(!poll_error_is_transient(&DeviceFlowError::Expired));
    }

    #[cfg(unix)]
    #[test]
    fn save_token_creates_0600_file() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().expect("tempdir");
        with_home(dir.path(), || {
            save_token(&StoredToken {
                access_token: "gho_secret".into(),
                token_type: "bearer".into(),
                scope: "repo".into(),
                obtained_at: None,
                expires_at: None,
            })
            .expect("save");
            let mode = std::fs::metadata(token_path())
                .expect("token file exists")
                .permissions()
                .mode();
            assert_eq!(
                mode & 0o777,
                0o600,
                "token file must never be group/world-readable"
            );
        });
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
