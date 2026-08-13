//! The rendezvous relay service — a separate, codefly-hosted deployable.
//!
//! ```text
//! lazybox-relay [BIND_ADDR]      default $LAZYBOX_RELAY_LISTEN_ADDR or 0.0.0.0:9443
//! lazybox-relay --healthcheck    TCP-connect to the configured listener
//! ```
//!
//! Boxes dial out and register; clients reach them by box-id; the relay
//! forwards ciphertext only and executes nothing.

use std::sync::Arc;

use lazybox_config::RelayEntitlementConfig;
use lazybox_entitlement::PlatformEntitlementGate;
use lazybox_relay::Relay;
use tokio::net::{TcpListener, TcpStream};

const DEFAULT_BIND: &str = "0.0.0.0:9443";
const LISTEN_ADDR_ENV: &str = "LAZYBOX_RELAY_LISTEN_ADDR";
const HEALTHCHECK_ADDR_ENV: &str = "LAZYBOX_RELAY_HEALTHCHECK_ADDR";

#[tokio::main]
async fn main() -> std::io::Result<()> {
    let args = std::env::args().skip(1).collect::<Vec<_>>();
    let listen_addr = resolve_listen_addr(
        args.first().filter(|arg| arg.as_str() != "--healthcheck"),
        std::env::var(LISTEN_ADDR_ENV).ok().as_ref(),
    );
    if matches!(args.first().map(String::as_str), Some("--healthcheck")) {
        let healthcheck_addr = args
            .get(1)
            .cloned()
            .or_else(|| std::env::var(HEALTHCHECK_ADDR_ENV).ok())
            .unwrap_or_else(|| healthcheck_addr_for(&listen_addr));
        TcpStream::connect(healthcheck_addr).await?;
        return Ok(());
    }

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "lazybox_relay=info".into()),
        )
        .init();

    let config = lazybox_config::Config::load().map_err(std::io::Error::other)?;
    let relay = relay_from_config(
        &config.relay.entitlement,
        std::env::var("LAZYBOX_PLATFORM_URL").ok(),
        std::env::var("LAZYBOX_PLATFORM_API_KEY").ok(),
    )?;
    let listener = TcpListener::bind(&listen_addr).await?;
    tracing::info!(%listen_addr, "lazybox-relay listening");
    Arc::new(relay).serve(listener).await
}

fn relay_from_config(
    config: &RelayEntitlementConfig,
    env_platform_url: Option<String>,
    env_api_key: Option<String>,
) -> std::io::Result<Relay> {
    let platform_url = env_platform_url
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .or_else(|| {
            config
                .platform_url
                .as_deref()
                .map(str::trim)
                .filter(|value| !value.is_empty())
        });
    let api_key = env_api_key
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .or_else(|| {
            config
                .api_key
                .as_deref()
                .map(str::trim)
                .filter(|value| !value.is_empty())
        });

    match (platform_url, api_key) {
        (None, None) => Ok(Relay::new()),
        (Some(platform_url), Some(api_key)) => Ok(Relay::with_gate(Box::new(
            PlatformEntitlementGate::new(platform_url, api_key),
        ))),
        (Some(_), None) => Err(std::io::Error::other(
            "relay entitlement platform URL requires LAZYBOX_PLATFORM_API_KEY or relay.entitlement.api_key",
        )),
        (None, Some(_)) => Err(std::io::Error::other(
            "relay entitlement API key requires LAZYBOX_PLATFORM_URL or relay.entitlement.platform_url",
        )),
    }
}

#[cfg(test)]
mod entitlement_config_tests {
    use super::*;

    #[test]
    fn entitlement_config_is_optional() {
        assert!(relay_from_config(&RelayEntitlementConfig::default(), None, None).is_ok());
    }

    #[test]
    fn entitlement_config_requires_url_and_key() {
        let only_url = RelayEntitlementConfig {
            platform_url: Some("https://platform.example".into()),
            api_key: None,
        };
        assert!(relay_from_config(&only_url, None, None).is_err());

        let only_key = RelayEntitlementConfig {
            platform_url: None,
            api_key: Some("secret".into()),
        };
        assert!(relay_from_config(&only_key, None, None).is_err());
    }

    #[test]
    fn environment_completes_platform_config_without_yaml() {
        assert!(
            relay_from_config(
                &RelayEntitlementConfig::default(),
                Some("https://platform.example".into()),
                Some("secret".into()),
            )
            .is_ok()
        );
    }
}

fn resolve_listen_addr(argument: Option<&String>, environment: Option<&String>) -> String {
    argument
        .or(environment)
        .map(|value| value.trim())
        .filter(|value| !value.is_empty())
        .unwrap_or(DEFAULT_BIND)
        .to_string()
}

fn healthcheck_addr_for(listen_addr: &str) -> String {
    match listen_addr.parse::<std::net::SocketAddr>() {
        Ok(addr) if addr.ip().is_unspecified() => {
            let loopback = match addr.ip() {
                std::net::IpAddr::V4(_) => std::net::Ipv4Addr::LOCALHOST.into(),
                std::net::IpAddr::V6(_) => std::net::Ipv6Addr::LOCALHOST.into(),
            };
            std::net::SocketAddr::new(loopback, addr.port()).to_string()
        }
        _ => listen_addr.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn listen_addr_prefers_argument_then_environment_then_default() {
        let argument = "127.0.0.1:1111".to_string();
        let environment = "127.0.0.1:2222".to_string();
        assert_eq!(
            resolve_listen_addr(Some(&argument), Some(&environment)),
            argument
        );
        assert_eq!(resolve_listen_addr(None, Some(&environment)), environment);
        assert_eq!(resolve_listen_addr(None, None), DEFAULT_BIND);
    }

    #[test]
    fn healthcheck_uses_loopback_for_wildcard_listener() {
        assert_eq!(healthcheck_addr_for("0.0.0.0:9443"), "127.0.0.1:9443");
        assert_eq!(healthcheck_addr_for("[::]:9443"), "[::1]:9443");
        assert_eq!(
            healthcheck_addr_for("relay.internal:9443"),
            "relay.internal:9443"
        );
    }

    #[tokio::test]
    async fn healthcheck_target_accepts_a_tcp_connection() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let accepted = tokio::spawn(async move { listener.accept().await.unwrap() });

        TcpStream::connect(healthcheck_addr_for(&addr.to_string()))
            .await
            .unwrap();
        accepted.await.unwrap();
    }
}
