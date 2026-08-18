//! `lazybox account …` — link this box to lazybox-platform.

use std::time::Duration;

use anyhow::Context;
use lazybox_config::{AccountConfig, Config};
use lazybox_identity::BoxIdentity;
use serde_json::Value;

const DEFAULT_PLATFORM_URL: &str = "https://platform.lazybox.ai";
const CLAIM_PATH: &str = "/v1/devices/claim";
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
const ERROR_BODY_LIMIT: usize = 8 * 1024;
const USAGE: &str = "usage:\n  \
    lazybox account claim <code> [--platform-url <url>] [--name <name>]\n  \
    lazybox account status";

pub async fn account_subcommand(args: &[String]) -> anyhow::Result<()> {
    match args.first().map(String::as_str) {
        Some("claim") => claim(&args[1..]).await,
        Some("status") if args.len() == 1 => status(),
        other => anyhow::bail!(
            "unknown `lazybox account` verb {:?}\n{USAGE}",
            other.unwrap_or("<none>")
        ),
    }
}

async fn claim(args: &[String]) -> anyhow::Result<()> {
    let mut args = args.to_vec();
    let platform_flag = crate::take_value(&mut args, "--platform-url");
    let name_flag = crate::take_value(&mut args, "--name");
    let code = match args.as_slice() {
        [code] if !code.trim().is_empty() => code.trim(),
        _ => anyhow::bail!("account claim needs exactly one non-empty claim code\n{USAGE}"),
    };
    let current = Config::load().context("load lazybox config")?;
    let platform_url = resolve_platform_url(
        platform_flag.as_deref(),
        std::env::var("LAZYBOX_PLATFORM_URL").ok().as_deref(),
        current.account.platform_url.as_deref(),
    );
    let name = name_flag
        .map(|name| name.trim().to_string())
        .filter(|name| !name.is_empty())
        .or_else(|| {
            std::env::var("HOSTNAME")
                .ok()
                .map(|name| name.trim().to_string())
                .filter(|name| !name.is_empty())
        })
        .unwrap_or_else(|| "lazybox".to_string());
    let identity = BoxIdentity::load_or_generate(lazybox_core::paths::identity_dir())
        .context("load or generate box identity")?;
    let client = platform_client()?;
    let receipt = claim_platform(
        &client,
        &platform_url,
        code,
        &identity.public_key_base64(),
        &name,
    )
    .await?;
    let account = receipt.into_config(platform_url);
    let saved = account.clone();
    if let Err(error) = Config::save_with(move |config| config.account = saved) {
        anyhow::bail!(claim_persistence_failure(&account, &error));
    }

    println!(
        "Claimed this box for organization {}.",
        organization_label(&account)
    );
    println!(
        "  platform:    {}",
        account.platform_url.as_deref().unwrap_or("unknown")
    );
    println!(
        "  plan:        {}",
        account.plan.as_deref().unwrap_or("unknown")
    );
    println!("  entitlement: {}", entitlement_label(&account));
    Ok(())
}

fn platform_client() -> anyhow::Result<reqwest::Client> {
    reqwest::Client::builder()
        .timeout(REQUEST_TIMEOUT)
        // A 307/308 preserves the POST body. Following one could send the
        // one-time claim code to a different origin selected by a compromised
        // or misconfigured endpoint, so claims require an explicit final URL.
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .context("build platform HTTP client")
}

fn status() -> anyhow::Result<()> {
    let config = Config::load().context("load lazybox config")?;
    print!("{}", format_status(&config.account));
    Ok(())
}

fn resolve_platform_url(flag: Option<&str>, env: Option<&str>, cached: Option<&str>) -> String {
    flag.or(env)
        .or(cached)
        .map(str::trim)
        .filter(|url| !url.is_empty())
        .unwrap_or(DEFAULT_PLATFORM_URL)
        .trim_end_matches('/')
        .to_string()
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ClaimReceipt {
    organization_id: String,
    organization_name: Option<String>,
    device_id: Option<String>,
    plan: Option<String>,
    entitlement_active: Option<bool>,
    entitlement_reason: Option<String>,
}

impl ClaimReceipt {
    fn from_value(value: &Value) -> anyhow::Result<Self> {
        let value = value.get("data").unwrap_or(value);
        let organization_id = first_string(
            value,
            &[
                &["organization_id"],
                &["org_id"],
                &["organization", "id"],
                &["org", "id"],
                &["device", "organization_id"],
                &["device", "org_id"],
            ],
        )
        .context("platform claim response is missing the organization id")?;
        let organization_name = first_string(
            value,
            &[
                &["organization_name"],
                &["org_name"],
                &["organization", "name"],
                &["org", "name"],
            ],
        );
        let device_id = first_string(value, &[&["device_id"], &["device", "id"], &["id"]]);
        let plan = first_string(
            value,
            &[
                &["plan"],
                &["plan", "name"],
                &["plan", "id"],
                &["entitlement", "plan"],
            ],
        );
        let entitlement_active = first_bool(
            value,
            &[
                &["entitled"],
                &["active"],
                &["entitlement_active"],
                &["entitlement", "active"],
                &["entitlement", "entitled"],
            ],
        )
        .or_else(|| {
            first_string(value, &[&["entitlement_state"], &["entitlement", "state"]]).and_then(
                |state| match state.to_ascii_lowercase().as_str() {
                    "active" | "entitled" => Some(true),
                    "inactive" | "unentitled" | "expired" => Some(false),
                    _ => None,
                },
            )
        });
        let entitlement_reason = first_string(
            value,
            &[
                &["entitlement_reason"],
                &["reason"],
                &["entitlement", "reason"],
            ],
        );
        Ok(Self {
            organization_id,
            organization_name,
            device_id,
            plan,
            entitlement_active,
            entitlement_reason,
        })
    }

    fn into_config(self, platform_url: String) -> AccountConfig {
        AccountConfig {
            platform_url: Some(platform_url),
            organization_id: Some(self.organization_id),
            organization_name: self.organization_name,
            device_id: self.device_id,
            plan: self.plan,
            entitlement_active: self.entitlement_active,
            entitlement_reason: self.entitlement_reason,
        }
    }
}

async fn claim_platform(
    client: &reqwest::Client,
    platform_url: &str,
    code: &str,
    device_public_key: &str,
    name: &str,
) -> anyhow::Result<ClaimReceipt> {
    let endpoint = format!("{}{CLAIM_PATH}", platform_url.trim_end_matches('/'));
    let mut response = client
        .post(&endpoint)
        .json(&serde_json::json!({
            "code": code,
            "device_public_key": device_public_key,
            "name": name,
        }))
        .send()
        .await
        .with_context(|| format!("claim box through {endpoint}"))?;
    let status = response.status();
    if !status.is_success() {
        let detail = platform_error_detail(&mut response).await;
        let suffix = detail
            .as_deref()
            .map(|detail| format!(": {detail}"))
            .unwrap_or_default();
        anyhow::bail!("platform rejected the claim with HTTP {status}{suffix}");
    }
    let value = response
        .json::<Value>()
        .await
        .context("parse platform claim response")?;
    ClaimReceipt::from_value(&value)
}

/// Read a bounded rejection body and extract only the platform's declared
/// error/message field. Claim responses may come from a user-configured URL,
/// so an unbounded `.text()` would let a bad endpoint consume arbitrary
/// memory, while dumping an unknown JSON object could expose unrelated
/// response fields. Plain-text errors are kept as a compact fallback.
async fn platform_error_detail(response: &mut reqwest::Response) -> Option<String> {
    let mut body = Vec::new();
    while body.len() < ERROR_BODY_LIMIT {
        let chunk = match response.chunk().await {
            Ok(Some(chunk)) => chunk,
            Ok(None) | Err(_) => break,
        };
        let remaining = ERROR_BODY_LIMIT - body.len();
        body.extend_from_slice(&chunk[..chunk.len().min(remaining)]);
    }
    platform_error_detail_from_bytes(&body)
}

fn platform_error_detail_from_bytes(body: &[u8]) -> Option<String> {
    if let Ok(value) = serde_json::from_slice::<Value>(body) {
        return first_string(
            &value,
            &[&["error"], &["message"], &["detail"], &["error", "message"]],
        )
        .map(|detail| compact_error_text(&detail));
    }
    // A bounded read can truncate otherwise-valid JSON. Never reinterpret a
    // JSON-looking parse failure as plain text: that could print an unrelated
    // field (for example a token) which the declared-field filter above was
    // specifically meant to suppress.
    if body
        .iter()
        .copied()
        .find(|byte| !byte.is_ascii_whitespace())
        .is_some_and(|byte| byte == b'{' || byte == b'[')
    {
        return None;
    }
    let text = String::from_utf8_lossy(body);
    let detail = compact_error_text(&text);
    (!detail.is_empty()).then_some(detail)
}

fn compact_error_text(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// A one-time claim code can be consumed even when the following local write
/// fails. Say explicitly that the remote side succeeded and retain the
/// non-secret association identifiers in the error so the operator can
/// recover instead of retrying a now-invalid code blindly.
fn claim_persistence_failure(account: &AccountConfig, error: &dyn std::fmt::Display) -> String {
    format!(
        "platform claim succeeded for organization {}, but lazybox could not persist the local association: {error}. The one-time claim code may now be consumed; fix the config-file error and claim again only if the platform still shows this device as unlinked",
        organization_label(account)
    )
}

fn value_at<'a>(value: &'a Value, path: &[&str]) -> Option<&'a Value> {
    path.iter().try_fold(value, |current, key| current.get(key))
}

fn first_string(value: &Value, paths: &[&[&str]]) -> Option<String> {
    paths.iter().find_map(|path| {
        value_at(value, path)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_string)
    })
}

fn first_bool(value: &Value, paths: &[&[&str]]) -> Option<bool> {
    paths
        .iter()
        .find_map(|path| value_at(value, path).and_then(Value::as_bool))
}

fn organization_label(account: &AccountConfig) -> String {
    match (
        account.organization_name.as_deref(),
        account.organization_id.as_deref(),
    ) {
        (Some(name), Some(id)) => format!("{name} ({id})"),
        (None, Some(id)) => id.to_string(),
        _ => "unknown".to_string(),
    }
}

fn entitlement_label(account: &AccountConfig) -> String {
    match account.entitlement_active {
        Some(true) => "active".to_string(),
        Some(false) => account
            .entitlement_reason
            .as_deref()
            .map(|reason| format!("inactive ({reason})"))
            .unwrap_or_else(|| "inactive".to_string()),
        None => "unknown (cached claim did not include entitlement state)".to_string(),
    }
}

fn format_status(account: &AccountConfig) -> String {
    let Some(platform) = account.platform_url.as_deref() else {
        return "Account: unlinked\nRun `lazybox account claim <code>` with a code from the platform UI.\n"
            .to_string();
    };
    if account.organization_id.is_none() {
        return format!(
            "Account: unlinked\nPlatform: {platform}\nRun `lazybox account claim <code>` to finish linking.\n"
        );
    }
    format!(
        "Account: linked (cached claim)\nPlatform: {platform}\nOrganization: {}\nPlan: {}\nEntitlement: {}\n",
        organization_label(account),
        account.plan.as_deref().unwrap_or("unknown"),
        entitlement_label(account),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use reqwest::StatusCode;
    use std::sync::{Arc, Mutex};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    async fn mock_platform(
        status: StatusCode,
        body: &'static str,
    ) -> (String, Arc<Mutex<Vec<u8>>>, tokio::task::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let request = Arc::new(Mutex::new(Vec::new()));
        let captured = Arc::clone(&request);
        let task = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut bytes = Vec::new();
            let header_end = loop {
                let mut chunk = [0u8; 4096];
                let read = stream.read(&mut chunk).await.unwrap();
                assert!(read > 0);
                bytes.extend_from_slice(&chunk[..read]);
                if let Some(index) = bytes.windows(4).position(|window| window == b"\r\n\r\n") {
                    break index + 4;
                }
            };
            let headers = String::from_utf8_lossy(&bytes[..header_end]);
            let content_length = headers
                .lines()
                .find_map(|line| {
                    let (name, value) = line.split_once(':')?;
                    name.eq_ignore_ascii_case("content-length")
                        .then(|| value.trim().parse::<usize>().ok())
                        .flatten()
                })
                .unwrap_or(0);
            while bytes.len() < header_end + content_length {
                let mut chunk = [0u8; 4096];
                let read = stream.read(&mut chunk).await.unwrap();
                assert!(read > 0);
                bytes.extend_from_slice(&chunk[..read]);
            }
            *captured.lock().unwrap() = bytes;
            let reason = status.canonical_reason().unwrap_or("Status");
            let reply = format!(
                "HTTP/1.1 {} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                status.as_u16(),
                body.len()
            );
            stream.write_all(reply.as_bytes()).await.unwrap();
        });
        (format!("http://{address}"), request, task)
    }

    #[test]
    fn platform_url_precedence_is_flag_then_env_then_cache_then_default() {
        assert_eq!(
            resolve_platform_url(
                Some("https://flag/"),
                Some("https://env"),
                Some("https://old")
            ),
            "https://flag"
        );
        assert_eq!(
            resolve_platform_url(None, Some("https://env/"), Some("https://old")),
            "https://env"
        );
        assert_eq!(
            resolve_platform_url(None, None, Some("https://old/")),
            "https://old"
        );
        assert_eq!(resolve_platform_url(None, None, None), DEFAULT_PLATFORM_URL);
    }

    #[test]
    fn receipt_accepts_flat_and_nested_platform_shapes_without_caching_secrets() {
        let receipt = ClaimReceipt::from_value(&serde_json::json!({
            "device": { "id": "dev_7" },
            "organization": { "id": "org_42", "name": "Example" },
            "plan": { "name": "pro" },
            "entitlement": { "state": "active", "reason": "paid" },
            "access_token": "must-not-persist"
        }))
        .unwrap();
        let account = receipt.into_config("https://platform.example".into());

        assert_eq!(account.organization_id.as_deref(), Some("org_42"));
        assert_eq!(account.device_id.as_deref(), Some("dev_7"));
        assert_eq!(account.plan.as_deref(), Some("pro"));
        assert_eq!(account.entitlement_active, Some(true));
        let json = serde_json::to_string(&account).unwrap();
        assert!(!json.contains("access_token"));
        assert!(!json.contains("claim"));
    }

    #[test]
    fn receipt_accepts_a_data_wrapped_platform_response() {
        let receipt = ClaimReceipt::from_value(&serde_json::json!({
            "data": {
                "device": { "id": "dev_8" },
                "org": { "id": "org_43", "name": "Wrapped" },
                "entitlement": { "active": false, "reason": "trial-ended" }
            }
        }))
        .unwrap();

        assert_eq!(receipt.organization_id, "org_43");
        assert_eq!(receipt.organization_name.as_deref(), Some("Wrapped"));
        assert_eq!(receipt.device_id.as_deref(), Some("dev_8"));
        assert_eq!(receipt.entitlement_active, Some(false));
        assert_eq!(receipt.entitlement_reason.as_deref(), Some("trial-ended"));
    }

    #[test]
    fn receipt_refuses_a_success_shape_without_an_org_association() {
        let error = ClaimReceipt::from_value(&serde_json::json!({"device_id":"dev_1"}))
            .unwrap_err()
            .to_string();
        assert!(error.contains("organization id"), "{error}");
    }

    #[test]
    fn status_is_explicit_for_unlinked_active_and_inactive_accounts() {
        assert!(format_status(&AccountConfig::default()).contains("unlinked"));
        let active = AccountConfig {
            platform_url: Some("https://platform.example".into()),
            organization_id: Some("org_1".into()),
            organization_name: Some("Acme".into()),
            plan: Some("pro".into()),
            entitlement_active: Some(true),
            ..AccountConfig::default()
        };
        let output = format_status(&active);
        assert!(output.contains("Acme (org_1)"));
        assert!(output.contains("Plan: pro"));
        assert!(output.contains("Entitlement: active"));

        let inactive = AccountConfig {
            entitlement_active: Some(false),
            entitlement_reason: Some("lapsed".into()),
            ..active
        };
        assert!(format_status(&inactive).contains("inactive (lapsed)"));
    }

    #[tokio::test]
    async fn claim_posts_only_the_frozen_public_contract() {
        let (url, seen, server) = mock_platform(
            StatusCode::OK,
            r#"{"device_id":"dev_7","org_id":"org_42","plan":"pro","entitled":true}"#,
        )
        .await;
        let client = platform_client().unwrap();
        let receipt = claim_platform(&client, &url, "ABCD1234", "Ym94LWtleQ==", "dev box")
            .await
            .unwrap();
        server.await.unwrap();

        assert_eq!(receipt.organization_id, "org_42");
        let request = seen.lock().unwrap();
        let header_end = request
            .windows(4)
            .position(|window| window == b"\r\n\r\n")
            .unwrap()
            + 4;
        let headers = String::from_utf8_lossy(&request[..header_end]);
        assert!(headers.starts_with("POST /v1/devices/claim HTTP/1.1"));
        assert!(!headers.to_ascii_lowercase().contains("authorization:"));
        let body: Value = serde_json::from_slice(&request[header_end..]).unwrap();
        assert_eq!(
            body,
            serde_json::json!({
                "code": "ABCD1234",
                "device_public_key": "Ym94LWtleQ==",
                "name": "dev box"
            })
        );
    }

    #[tokio::test]
    async fn rejected_claim_is_an_error_and_never_looks_linked() {
        let (url, _, server) = mock_platform(StatusCode::FORBIDDEN, r#"{"error":"expired"}"#).await;
        let error = claim_platform(
            &platform_client().unwrap(),
            &url,
            "EXPIRED",
            "public-key",
            "box",
        )
        .await
        .unwrap_err()
        .to_string();
        server.await.unwrap();
        assert!(error.contains("HTTP 403"), "{error}");
        assert!(error.contains("expired"), "{error}");
    }

    #[tokio::test]
    async fn claim_never_forwards_the_one_time_code_through_a_redirect() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = [0u8; 4096];
            let _ = stream.read(&mut request).await.unwrap();
            stream
                .write_all(
                    b"HTTP/1.1 307 Temporary Redirect\r\nLocation: http://127.0.0.1:1/stolen\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                )
                .await
                .unwrap();
        });

        let error = claim_platform(
            &platform_client().unwrap(),
            &format!("http://{address}"),
            "ONE-TIME-CODE",
            "public-key",
            "box",
        )
        .await
        .unwrap_err()
        .to_string();
        server.await.unwrap();

        assert!(error.contains("HTTP 307"), "{error}");
        assert!(
            !error.contains("127.0.0.1:1"),
            "the redirect target should never be contacted: {error}"
        );
    }

    #[test]
    fn rejection_detail_is_bounded_to_declared_json_fields() {
        assert_eq!(
            platform_error_detail_from_bytes(
                br#"{"error":{"message":"code expired"},"token":"must-not-print"}"#,
            )
            .as_deref(),
            Some("code expired")
        );
        assert_eq!(
            platform_error_detail_from_bytes(b"  service   unavailable\n").as_deref(),
            Some("service unavailable")
        );
        assert_eq!(
            platform_error_detail_from_bytes(br#"{"token":"must-not-print""#),
            None,
            "truncated JSON must not fall through to the plain-text path"
        );
    }

    #[test]
    fn post_claim_persistence_failure_never_masquerades_as_remote_rejection() {
        let account = AccountConfig {
            organization_id: Some("org_42".into()),
            organization_name: Some("Example".into()),
            ..AccountConfig::default()
        };
        let message = claim_persistence_failure(&account, &"disk full");
        assert!(message.contains("platform claim succeeded"), "{message}");
        assert!(message.contains("Example (org_42)"), "{message}");
        assert!(message.contains("one-time claim code"), "{message}");
    }
}
