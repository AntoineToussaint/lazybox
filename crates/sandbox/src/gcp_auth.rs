//! Native ADC access-token minting for the GCP provider (#1126).
//!
//! Replaces `gcloud auth print-access-token` (and the ambient ADC gcloud/
//! terraform both leaned on) with a direct Application Default Credentials
//! resolution + OAuth2 token exchange over `reqwest`. Three credential
//! sources are honored, in the same precedence gcloud's ADC uses:
//!
//! 1. an explicit `service_account_key` (JWT-bearer grant, RS256-signed),
//! 2. an ambient authorized-user credential — the well-known ADC file a
//!    `gcloud auth application-default login` writes (refresh-token grant),
//! 3. the GCE metadata server (when running on a box).
//!
//! An `impersonate_service_account` target is layered on top of whichever
//! base resolves, via the IAM Credentials `generateAccessToken` API.
//!
//! The load-bearing reason this exists: a **stale** authorized-user
//! credential fails the refresh with `invalid_grant` /
//! `reauth related error (invalid_rapt)`. gcloud/terraform surfaced that as
//! a raw, un-actionable error deep in a provision. Here it is classified
//! into [`SandboxError::ReauthRequired`] so the UI can prompt a re-login.
//!
//! Every request/response shaping step is a pure function so the token
//! exchange is unit-tested without a live GCP endpoint; only the thin
//! `reqwest` send/await glue is untested here (it is exercised by the real
//! acceptance run).

use std::path::PathBuf;
use std::time::Duration;

use jsonwebtoken::{Algorithm, EncodingKey, Header};
use serde::{Deserialize, Serialize};

use crate::gcp::GcpAuth;
use crate::provider::SandboxError;

const TOKEN_URI: &str = "https://oauth2.googleapis.com/token";
const METADATA_TOKEN_URI: &str =
    "http://metadata.google.internal/computeMetadata/v1/instance/service-accounts/default/token";
/// The scope every lifecycle op needs — full Compute + IAM read/write.
const CLOUD_PLATFORM_SCOPE: &str = "https://www.googleapis.com/auth/cloud-platform";
/// SA-key JWT-bearer assertion lifetime; Google caps it at one hour.
const JWT_LIFETIME_SECS: i64 = 3600;
/// Bound on every credential-minting request. Without it a stuck endpoint —
/// most sharply the metadata server dialed from a machine that isn't on GCE,
/// where the connect can hang rather than refuse — would block `status` /
/// `check_auth` indefinitely instead of failing fast (#1126).
const TOKEN_REQUEST_TIMEOUT: Duration = Duration::from_secs(20);

/// A resolved ADC credential source, before any impersonation is layered on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AdcSource {
    /// A `service_account` key JSON — minted via a signed JWT-bearer grant.
    ServiceAccount(Box<ServiceAccountKey>),
    /// An `authorized_user` credential (a `gcloud auth application-default
    /// login` result) — minted via its refresh token. This is the source
    /// whose stale refresh yields `invalid_rapt`.
    AuthorizedUser(AuthorizedUser),
    /// No file credential; mint from the GCE metadata server.
    Metadata,
}

/// The fields of a `service_account` key JSON needed to mint a token.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct ServiceAccountKey {
    pub client_email: String,
    pub private_key: String,
    #[serde(default)]
    pub private_key_id: Option<String>,
    #[serde(default)]
    pub token_uri: Option<String>,
}

/// The fields of an `authorized_user` (ADC login) JSON.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct AuthorizedUser {
    pub client_id: String,
    pub client_secret: String,
    pub refresh_token: String,
}

/// The `type`-tagged shape both ADC credential JSONs share, used only to
/// dispatch parsing to the right concrete struct.
#[derive(Deserialize)]
struct AdcTag {
    #[serde(rename = "type")]
    kind: String,
}

/// Parse an ADC credential file's JSON into a typed source. Both the
/// `service_account` and `authorized_user` shapes carry a `type` discriminant;
/// anything else is a config error naming the unknown type.
pub fn parse_adc_json(json: &str) -> Result<AdcSource, SandboxError> {
    let tag: AdcTag = serde_json::from_str(json).map_err(|e| SandboxError::Parse {
        what: "ADC credential",
        detail: e.to_string(),
    })?;
    match tag.kind.as_str() {
        "service_account" => {
            let key: ServiceAccountKey =
                serde_json::from_str(json).map_err(|e| SandboxError::Parse {
                    what: "service_account key",
                    detail: e.to_string(),
                })?;
            Ok(AdcSource::ServiceAccount(Box::new(key)))
        }
        "authorized_user" => {
            let user: AuthorizedUser =
                serde_json::from_str(json).map_err(|e| SandboxError::Parse {
                    what: "authorized_user credential",
                    detail: e.to_string(),
                })?;
            Ok(AdcSource::AuthorizedUser(user))
        }
        other => Err(SandboxError::Config(format!(
            "unsupported ADC credential type {other:?} (expected service_account or authorized_user)"
        ))),
    }
}

/// The well-known ADC file path gcloud writes for
/// `application-default login`. Honors an explicit scoped config dir
/// (`CLOUDSDK_CONFIG`, #1047) so lazybox reads its own isolated login when
/// one is configured, else falls back to `~/.config/gcloud`.
pub fn well_known_adc_path(config_dir: Option<&PathBuf>) -> Option<PathBuf> {
    let base = match config_dir {
        Some(dir) => dir.clone(),
        None => dirs_config_gcloud()?,
    };
    Some(base.join("application_default_credentials.json"))
}

/// `~/.config/gcloud`, the default `CLOUDSDK_CONFIG`. `$HOME`-derived so it
/// matches gcloud without depending on the `dirs` crate.
fn dirs_config_gcloud() -> Option<PathBuf> {
    std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config").join("gcloud"))
}

/// Resolve which ADC source [`GcpAuth`] points at, reading whatever file is
/// implied. Precedence mirrors gcloud's ADC: an explicit key, then
/// `GOOGLE_APPLICATION_CREDENTIALS`, then the well-known login file, then the
/// metadata server.
fn resolve_adc(auth: &GcpAuth) -> Result<AdcSource, SandboxError> {
    if let Some(key) = &auth.service_account_key {
        return read_adc_file(key);
    }
    if let Some(path) = std::env::var_os("GOOGLE_APPLICATION_CREDENTIALS") {
        return read_adc_file(&PathBuf::from(path));
    }
    if let Some(path) = well_known_adc_path(auth.config_dir.as_ref())
        && path.exists()
    {
        return read_adc_file(&path);
    }
    Ok(AdcSource::Metadata)
}

fn read_adc_file(path: &PathBuf) -> Result<AdcSource, SandboxError> {
    let json = std::fs::read_to_string(path).map_err(|e| {
        SandboxError::Config(format!(
            "gcp credentials: reading ADC file {} failed: {e}",
            path.display()
        ))
    })?;
    parse_adc_json(&json)
}

/// The claims of the SA-key JWT-bearer assertion. Pure + serializable so the
/// assertion is asserted-on in tests with a fixed clock.
#[derive(Debug, Serialize, PartialEq, Eq)]
pub struct JwtClaims {
    pub iss: String,
    pub scope: String,
    pub aud: String,
    pub iat: i64,
    pub exp: i64,
}

impl JwtClaims {
    /// Build the assertion claims for `key`, valid from `iat` for
    /// `JWT_LIFETIME_SECS`. `aud` is the key's own `token_uri` when it
    /// carries one, else the standard endpoint.
    pub fn for_key(key: &ServiceAccountKey, iat: i64) -> Self {
        Self {
            iss: key.client_email.clone(),
            scope: CLOUD_PLATFORM_SCOPE.to_string(),
            aud: key
                .token_uri
                .clone()
                .unwrap_or_else(|| TOKEN_URI.to_string()),
            iat,
            exp: iat + JWT_LIFETIME_SECS,
        }
    }
}

/// Sign the JWT-bearer assertion for `key` with RS256. Separated from the
/// exchange so a signing failure (a malformed private key) is a precise
/// config error rather than a network-shaped one.
fn sign_assertion(key: &ServiceAccountKey, iat: i64) -> Result<String, SandboxError> {
    let claims = JwtClaims::for_key(key, iat);
    let mut header = Header::new(Algorithm::RS256);
    header.kid = key.private_key_id.clone();
    let encoding = EncodingKey::from_rsa_pem(key.private_key.as_bytes()).map_err(|e| {
        SandboxError::Config(format!(
            "gcp credentials: service_account private_key is not valid RSA PEM: {e}"
        ))
    })?;
    jsonwebtoken::encode(&header, &claims, &encoding).map_err(|e| {
        SandboxError::Config(format!(
            "gcp credentials: signing the JWT assertion failed: {e}"
        ))
    })
}

/// The `application/x-www-form-urlencoded` body of a refresh-token grant.
pub fn refresh_grant_form(user: &AuthorizedUser) -> Vec<(&'static str, String)> {
    vec![
        ("client_id", user.client_id.clone()),
        ("client_secret", user.client_secret.clone()),
        ("refresh_token", user.refresh_token.clone()),
        ("grant_type", "refresh_token".to_string()),
    ]
}

/// The form body of a JWT-bearer grant carrying a signed `assertion`.
pub fn jwt_bearer_form(assertion: String) -> Vec<(&'static str, String)> {
    vec![
        (
            "grant_type",
            "urn:ietf:params:oauth:grant-type:jwt-bearer".to_string(),
        ),
        ("assertion", assertion),
    ]
}

/// A freshly minted token plus its reported lifetime, so callers can cache it
/// and stop re-minting on every op (#1126). `ttl` is the endpoint's
/// `expires_in`; `None` when it wasn't reported.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MintedToken {
    pub value: String,
    pub ttl: Option<Duration>,
}

/// A successful token-endpoint response.
#[derive(Debug, Deserialize)]
struct TokenOk {
    access_token: String,
    #[serde(default)]
    expires_in: Option<u64>,
}

/// An OAuth2 token-endpoint error body (`{error, error_description}`).
#[derive(Debug, Deserialize)]
struct TokenErr {
    error: String,
    #[serde(default)]
    error_description: String,
}

/// True when a token-endpoint error is the "user must log in again" signal.
/// `invalid_rapt` / an explicit `reauth` are source-agnostic re-login prompts.
/// A **bare** `invalid_grant` is a re-login signal *only for the user
/// (refresh-token) grant* (`user_grant`), where it means a revoked/expired
/// login; on the service-account JWT-bearer path the same code means clock
/// skew or a bad assertion — which a re-login can't fix — so it must NOT be
/// classified as reauth there (#1126 review).
pub fn is_reauth_error(error: &str, description: &str, user_grant: bool) -> bool {
    let desc = description.to_ascii_lowercase();
    desc.contains("invalid_rapt")
        || desc.contains("reauth")
        || (user_grant && error == "invalid_grant")
}

/// Interpret a token-endpoint response body. `success` is the transport's
/// 2xx verdict; `user_grant` says whether this was a refresh-token grant (see
/// [`is_reauth_error`]). On failure the OAuth error is classified, mapping the
/// reauth case to [`SandboxError::ReauthRequired`] and everything else to a
/// transport-shaped API error.
pub fn parse_token_response(
    success: bool,
    body: &str,
    user_grant: bool,
) -> Result<MintedToken, SandboxError> {
    if success {
        let ok: TokenOk = serde_json::from_str(body).map_err(|e| SandboxError::Parse {
            what: "token response",
            detail: e.to_string(),
        })?;
        return Ok(MintedToken {
            value: ok.access_token,
            ttl: ok.expires_in.map(Duration::from_secs),
        });
    }
    // A non-2xx body is normally the OAuth error JSON; fall back to the raw
    // body when it isn't so the detail is never lost.
    match serde_json::from_str::<TokenErr>(body) {
        Ok(err) if is_reauth_error(&err.error, &err.error_description, user_grant) => {
            Err(SandboxError::ReauthRequired {
                detail: format!("{}: {}", err.error, err.error_description),
            })
        }
        Ok(err) => Err(SandboxError::Api {
            provider: "gcp",
            operation: "oauth token",
            status: 400,
            detail: format!("{}: {}", err.error, err.error_description),
        }),
        Err(_) => Err(SandboxError::Api {
            provider: "gcp",
            operation: "oauth token",
            status: 400,
            detail: body.trim().to_string(),
        }),
    }
}

/// The IAM Credentials `generateAccessToken` endpoint for `target`.
pub fn impersonate_uri(target: &str) -> String {
    format!(
        "https://iamcredentials.googleapis.com/v1/projects/-/serviceAccounts/{target}:generateAccessToken"
    )
}

#[derive(Debug, Serialize)]
struct ImpersonateBody {
    scope: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct ImpersonateOk {
    #[serde(rename = "accessToken")]
    access_token: String,
    /// RFC3339 expiry (`expireTime`); used to derive the cache TTL.
    #[serde(rename = "expireTime", default)]
    expire_time: Option<String>,
}

impl GcpAuth {
    /// Mint an OAuth2 access token for the Compute/IAM APIs from whatever
    /// this config resolves to — natively, with no `gcloud` on PATH (#1126).
    /// Returns the token with its lifetime so the caller can cache it; a stale
    /// login surfaces as [`SandboxError::ReauthRequired`].
    pub async fn access_token(
        &self,
        client: &reqwest::Client,
    ) -> Result<MintedToken, SandboxError> {
        let base = self.mint_base_token(client).await?;
        match &self.impersonate_service_account {
            Some(target) => impersonate(client, &base.value, target).await,
            None => Ok(base),
        }
    }

    /// Mint the base (pre-impersonation) token from the resolved ADC source.
    async fn mint_base_token(&self, client: &reqwest::Client) -> Result<MintedToken, SandboxError> {
        match resolve_adc(self)? {
            AdcSource::ServiceAccount(key) => {
                let assertion = sign_assertion(&key, unix_now())?;
                let token_uri = key.token_uri.as_deref().unwrap_or(TOKEN_URI);
                // JWT-bearer, not a user login → bare invalid_grant is not reauth.
                post_token(client, token_uri, jwt_bearer_form(assertion), false).await
            }
            AdcSource::AuthorizedUser(user) => {
                // Refresh-token (user login) grant → bare invalid_grant is reauth.
                post_token(client, TOKEN_URI, refresh_grant_form(&user), true).await
            }
            AdcSource::Metadata => metadata_token(client).await,
        }
    }
}

/// Seconds since the Unix epoch, for the JWT `iat`/`exp`. Isolated so the
/// pure claim builder stays clock-free and testable.
fn unix_now() -> i64 {
    chrono::Utc::now().timestamp()
}

/// The lifetime remaining until an RFC3339 instant, for the impersonation
/// `expireTime`. `None` on an unparseable stamp or one already in the past,
/// so a bogus/expired value falls back to the default TTL rather than caching
/// a dead token.
fn ttl_until_rfc3339(stamp: &str) -> Option<Duration> {
    let expires = chrono::DateTime::parse_from_rfc3339(stamp).ok()?;
    (expires.timestamp() - unix_now())
        .try_into()
        .ok()
        .map(Duration::from_secs)
}

/// POST a form-encoded grant to a token endpoint and interpret the result.
/// `user_grant` distinguishes a refresh-token login from a JWT-bearer grant
/// for reauth classification (see [`is_reauth_error`]).
async fn post_token(
    client: &reqwest::Client,
    uri: &str,
    form: Vec<(&'static str, String)>,
    user_grant: bool,
) -> Result<MintedToken, SandboxError> {
    let response = client
        .post(uri)
        .timeout(TOKEN_REQUEST_TIMEOUT)
        .form(&form)
        .send()
        .await
        .map_err(|e| SandboxError::ApiTransport {
            provider: "gcp",
            operation: "oauth token",
            detail: e.to_string(),
        })?;
    let success = response.status().is_success();
    let body = response
        .text()
        .await
        .map_err(|e| SandboxError::ApiTransport {
            provider: "gcp",
            operation: "oauth token",
            detail: e.to_string(),
        })?;
    parse_token_response(success, &body, user_grant)
}

/// Mint a token from the GCE metadata server. Not a user login, so a bare
/// `invalid_grant` here is never a reauth prompt.
async fn metadata_token(client: &reqwest::Client) -> Result<MintedToken, SandboxError> {
    let response = client
        .get(METADATA_TOKEN_URI)
        .timeout(TOKEN_REQUEST_TIMEOUT)
        .header("Metadata-Flavor", "Google")
        .send()
        .await
        .map_err(|e| SandboxError::ApiTransport {
            provider: "gcp",
            operation: "metadata token",
            detail: e.to_string(),
        })?;
    let success = response.status().is_success();
    let body = response
        .text()
        .await
        .map_err(|e| SandboxError::ApiTransport {
            provider: "gcp",
            operation: "metadata token",
            detail: e.to_string(),
        })?;
    parse_token_response(success, &body, false)
}

/// Exchange a base token for an impersonated token via IAM Credentials.
async fn impersonate(
    client: &reqwest::Client,
    base_token: &str,
    target: &str,
) -> Result<MintedToken, SandboxError> {
    let response = client
        .post(impersonate_uri(target))
        .timeout(TOKEN_REQUEST_TIMEOUT)
        .bearer_auth(base_token)
        .json(&ImpersonateBody {
            scope: vec![CLOUD_PLATFORM_SCOPE.to_string()],
        })
        .send()
        .await
        .map_err(|e| SandboxError::ApiTransport {
            provider: "gcp",
            operation: "impersonate",
            detail: e.to_string(),
        })?;
    let status = response.status();
    let body = response
        .text()
        .await
        .map_err(|e| SandboxError::ApiTransport {
            provider: "gcp",
            operation: "impersonate",
            detail: e.to_string(),
        })?;
    if !status.is_success() {
        return Err(SandboxError::Api {
            provider: "gcp",
            operation: "impersonate",
            status: status.as_u16(),
            detail: body.trim().to_string(),
        });
    }
    let ok: ImpersonateOk = serde_json::from_str(&body).map_err(|e| SandboxError::Parse {
        what: "impersonate response",
        detail: e.to_string(),
    })?;
    Ok(MintedToken {
        value: ok.access_token,
        ttl: ok.expire_time.as_deref().and_then(ttl_until_rfc3339),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_service_account_key() {
        let json = r#"{
            "type": "service_account",
            "client_email": "sa@p.iam.gserviceaccount.com",
            "private_key": "-----BEGIN PRIVATE KEY-----\nMII...\n-----END PRIVATE KEY-----\n",
            "private_key_id": "kid-1"
        }"#;
        let source = parse_adc_json(json).unwrap();
        let AdcSource::ServiceAccount(key) = source else {
            panic!("expected service_account, got {source:?}");
        };
        assert_eq!(key.client_email, "sa@p.iam.gserviceaccount.com");
        assert_eq!(key.private_key_id.as_deref(), Some("kid-1"));
    }

    #[test]
    fn parses_an_authorized_user() {
        let json = r#"{
            "type": "authorized_user",
            "client_id": "cid.apps.googleusercontent.com",
            "client_secret": "secret",
            "refresh_token": "1//refresh"
        }"#;
        let source = parse_adc_json(json).unwrap();
        assert_eq!(
            source,
            AdcSource::AuthorizedUser(AuthorizedUser {
                client_id: "cid.apps.googleusercontent.com".into(),
                client_secret: "secret".into(),
                refresh_token: "1//refresh".into(),
            })
        );
    }

    #[test]
    fn rejects_an_unknown_adc_type() {
        let err = parse_adc_json(r#"{"type": "external_account"}"#).unwrap_err();
        assert!(matches!(err, SandboxError::Config(_)), "{err:?}");
        assert!(err.to_string().contains("external_account"), "{err}");
    }

    #[test]
    fn well_known_path_prefers_the_scoped_config_dir() {
        let scoped = PathBuf::from("/scoped/gcloud");
        let path = well_known_adc_path(Some(&scoped)).unwrap();
        assert_eq!(
            path,
            PathBuf::from("/scoped/gcloud/application_default_credentials.json")
        );
    }

    #[test]
    fn refresh_grant_form_carries_the_refresh_token_grant() {
        let user = AuthorizedUser {
            client_id: "cid".into(),
            client_secret: "sec".into(),
            refresh_token: "rt".into(),
        };
        let form = refresh_grant_form(&user);
        assert!(form.contains(&("grant_type", "refresh_token".to_string())));
        assert!(form.contains(&("refresh_token", "rt".to_string())));
        assert!(form.contains(&("client_id", "cid".to_string())));
    }

    #[test]
    fn jwt_bearer_form_carries_the_assertion_grant() {
        let form = jwt_bearer_form("signed.jwt.here".to_string());
        assert!(form.contains(&(
            "grant_type",
            "urn:ietf:params:oauth:grant-type:jwt-bearer".to_string()
        )));
        assert!(form.contains(&("assertion", "signed.jwt.here".to_string())));
    }

    #[test]
    fn jwt_claims_bound_the_scope_issuer_and_hour_lifetime() {
        let key = ServiceAccountKey {
            client_email: "sa@p.iam.gserviceaccount.com".into(),
            private_key: "unused".into(),
            private_key_id: None,
            token_uri: None,
        };
        let claims = JwtClaims::for_key(&key, 1_000);
        assert_eq!(claims.iss, "sa@p.iam.gserviceaccount.com");
        assert_eq!(claims.scope, CLOUD_PLATFORM_SCOPE);
        assert_eq!(claims.aud, TOKEN_URI);
        assert_eq!(claims.iat, 1_000);
        assert_eq!(claims.exp, 1_000 + JWT_LIFETIME_SECS);
    }

    #[test]
    fn invalid_rapt_is_classified_as_reauth_regardless_of_grant() {
        // The exact dogfooding failure: gcloud's stale ADC surfaced as this.
        // `invalid_rapt` is a re-login prompt on either grant.
        assert!(is_reauth_error(
            "invalid_grant",
            "reauth related error (invalid_rapt)",
            false
        ));
        assert!(is_reauth_error(
            "invalid_grant",
            "reauth related error (invalid_rapt)",
            true
        ));
    }

    #[test]
    fn bare_invalid_grant_is_reauth_only_for_the_user_grant() {
        // A refresh-token login → re-login. A service-account JWT-bearer grant
        // → clock skew / bad assertion, which a re-login can't fix, so NOT
        // reauth (#1126 review).
        assert!(is_reauth_error("invalid_grant", "", true));
        assert!(!is_reauth_error("invalid_grant", "", false));
        // A genuinely different error is never a reauth prompt.
        assert!(!is_reauth_error("invalid_scope", "bad scope", true));
    }

    #[test]
    fn token_success_extracts_the_access_token_and_ttl() {
        let minted = parse_token_response(
            true,
            r#"{"access_token":"ya29.abc","expires_in":3599}"#,
            true,
        )
        .unwrap();
        assert_eq!(minted.value, "ya29.abc");
        assert_eq!(minted.ttl, Some(Duration::from_secs(3599)));
    }

    #[test]
    fn token_success_without_expiry_reports_no_ttl() {
        let minted = parse_token_response(true, r#"{"access_token":"ya29.abc"}"#, false).unwrap();
        assert_eq!(minted.value, "ya29.abc");
        assert_eq!(minted.ttl, None);
    }

    #[test]
    fn stale_login_maps_to_reauth_required_not_a_raw_error() {
        let body = r#"{"error":"invalid_grant","error_description":"reauth related error (invalid_rapt)"}"#;
        let err = parse_token_response(false, body, true).unwrap_err();
        assert!(
            matches!(err, SandboxError::ReauthRequired { .. }),
            "{err:?}"
        );
        assert!(err.to_string().contains("re-authenticate"), "{err}");
    }

    #[test]
    fn service_account_invalid_grant_is_not_a_reauth_prompt() {
        // Same OAuth code, JWT-bearer path: must be a plain error, never a
        // "re-authenticate" prompt a headless deploy can't act on.
        let body =
            r#"{"error":"invalid_grant","error_description":"Invalid JWT: Token used too early"}"#;
        let err = parse_token_response(false, body, false).unwrap_err();
        assert!(matches!(err, SandboxError::Api { .. }), "{err:?}");
    }

    #[test]
    fn a_non_reauth_token_error_stays_a_plain_api_error() {
        let body = r#"{"error":"invalid_client","error_description":"bad client"}"#;
        let err = parse_token_response(false, body, true).unwrap_err();
        assert!(matches!(err, SandboxError::Api { .. }), "{err:?}");
    }

    #[test]
    fn a_non_json_error_body_is_preserved_verbatim() {
        let err = parse_token_response(false, "  upstream 502  ", true).unwrap_err();
        match err {
            SandboxError::Api { detail, .. } => assert_eq!(detail, "upstream 502"),
            other => panic!("expected Api, got {other:?}"),
        }
    }

    #[test]
    fn ttl_until_rfc3339_is_none_for_a_past_or_bogus_stamp() {
        assert_eq!(ttl_until_rfc3339("1999-01-01T00:00:00Z"), None);
        assert_eq!(ttl_until_rfc3339("not-a-timestamp"), None);
        // A far-future stamp yields a positive lifetime.
        assert!(ttl_until_rfc3339("2999-01-01T00:00:00Z").is_some());
    }

    #[test]
    fn impersonate_uri_targets_generate_access_token() {
        let uri = impersonate_uri("deploy@p.iam.gserviceaccount.com");
        assert_eq!(
            uri,
            "https://iamcredentials.googleapis.com/v1/projects/-/serviceAccounts/deploy@p.iam.gserviceaccount.com:generateAccessToken"
        );
    }

    #[test]
    fn a_real_rsa_key_signs_a_verifiable_assertion() {
        // Exercises the actual RS256 signing path end to end with a generated
        // key, so a broken header/claims/encoding combination fails here
        // rather than only against the live token endpoint.
        let key = ServiceAccountKey {
            client_email: "sa@p.iam.gserviceaccount.com".into(),
            private_key: TEST_RSA_PEM.to_string(),
            private_key_id: Some("kid-1".into()),
            token_uri: None,
        };
        let jwt = sign_assertion(&key, 1_000).expect("signs with a valid RSA key");
        // header.payload.signature
        assert_eq!(jwt.split('.').count(), 3, "{jwt}");
    }

    #[test]
    fn a_malformed_private_key_is_a_precise_config_error() {
        let key = ServiceAccountKey {
            client_email: "sa@p.iam.gserviceaccount.com".into(),
            private_key: "-----BEGIN PRIVATE KEY-----\nnot base64\n-----END PRIVATE KEY-----\n"
                .into(),
            private_key_id: None,
            token_uri: None,
        };
        let err = sign_assertion(&key, 1_000).unwrap_err();
        assert!(matches!(err, SandboxError::Config(_)), "{err:?}");
    }

    /// A throwaway 2048-bit RSA key in PKCS#8 PEM, for signing tests only.
    const TEST_RSA_PEM: &str = "-----BEGIN PRIVATE KEY-----
MIIEvQIBADANBgkqhkiG9w0BAQEFAASCBKcwggSjAgEAAoIBAQCsiSWWWBnz6lsE
crUhUf1Racu4SAemziEfn4oExFl+bk8SHSywlOgLyRUGoBvZuS1TEV48jfLmQRRj
eZ/XBeHw/z3+w26lhJ/JIjQ/R89qYVj9GnugUdRbf9h0TYMiDObLHaIGp1K+WMQu
0bX/Z4MgE8Sqi0QsSukODriQcyPx77NuB8JiWclWd4RlHHJ6W6trTnC/bbAxiUMT
kBaDv4NVBjfyCfBk96n6i1oLyEG4HVC39pwfSQJ6EQbYMaNKkKIqml+paIftwgHN
qatQXiGlHAhrFkFoVzH+9ceZEv1K+l3VnIfQfV15TtiIHfDwOHubxQxh0VUjKfpo
fwGM2vQFAgMBAAECggEACYTci8WrEuO+Z0mr2B2FqBj56h4hO/D571x2OSqbFGhi
GOn+pfRlNAdBT3cEalf22fMmm5kqSk1TYmSarrqOm2T9puFh6KRfS3HFZ6te1Gvw
BLDSGseA+5ZbWxlOr5F+Jz0ojAztuf48PqgHzkJH//xPOKiL15S/dGzX/nI3zLh5
WZjiQLU97IL5daGth7g87M1qnYJ9n+18YAyUqbrVzr5JTxEz9GtLZuvN3TN8b5dc
ukEcLhTeYHakwg1o5HM4d2flBbtaCcCAK2SQXYdGFYAkqorMPEtMB4N3t3uNcoN/
d5czC43x28LLM8iylQtVzX9V9jBm97UXVCSgYWNGoQKBgQDdJxCCJHNU/7L+3qAX
7xy+4yE272EaGhY0VSJOzBL8cKU0AQU5MP77m0tlml8lihYQXezbdTGrgwJHfWHc
+grjcdatmgCtWbapTHZeoKY46/qUcgKnmnLlf2ppU8UK+SAEhm/5C3/UqwuXLaIv
6ytuXzgkHJgndvNUcipe5/F2YQKBgQDHuPUIBN6+1uPRXSDHREQio5aH7fSKKtdY
DmJmyUAgnS56LCL/1ijfxrGWqo7R3kjutaGSs2XqGifrXJHc3+wuypSwQ3QeZg5c
Pzt+hiXtKgrxr3IZ143KxKWQ7r+a+0jRauGItzhgYZt1bB67AI+aP9CvTxeSc1+f
PLpgDfPYJQKBgCJB8uS2EMeR9IBWrCYI/EL1nCeOXVRVxirFJVNfeXFxYaX0ooKB
fH4tSDis+SAvi8ttQUudk9wlpyy713ULprQk5kRuHry7sPf4yD1QVW9r3p0wLAka
e8HhQvKd72ALx1HsIVxK07p15I2+m+kgXNH0HhY597flTTw/kyCwfU8BAoGAUQwy
i02GosULcDOtkI+YYsIl3QVoXgVim/5CWlnC5zlB2hw9G7rpnV3BRVXzXSEqUYxt
g92/Ns/kcTOAkbRg4OAelKTa41cg7rhOSVrg2yxbgpZi5C+//4/rbDQmlxrwAuOv
oE5R13LkpjL5CYQBwmOxKOMcuraNEE6Rh1dCI+0CgYEAgqSweTeci+KZEWmqehvs
b1BmC6oGoq/z2Yp6TOCsfkK7J/brUmleIf1pDe6vq3X/3Gf71yUuUWw4rknPVql5
i6BvWV7ntlUiDLyZ3l2P7c4SPwFjHs02sMfgrV+unml8X8q7zF2YCRAc31R1AQrS
AyYOm/CP8Hi7ZRm4nmTZhqU=
-----END PRIVATE KEY-----
";
}
