//! The rendezvous relay service — a separate, codefly-hosted deployable.
//!
//! ```text
//! lazybox-relay [BIND_ADDR]      default 0.0.0.0:9443
//! ```
//!
//! Boxes dial out and register; clients reach them by box-id; the relay
//! forwards ciphertext only and executes nothing.

use std::sync::Arc;

use lazybox_config::RelayEntitlementConfig;
use lazybox_entitlement::PlatformEntitlementGate;
use lazybox_relay::Relay;
use tokio::net::TcpListener;

const DEFAULT_BIND: &str = "0.0.0.0:9443";

#[tokio::main]
async fn main() -> std::io::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "lazybox_relay=info".into()),
        )
        .init();

    let bind = std::env::args()
        .nth(1)
        .unwrap_or_else(|| DEFAULT_BIND.into());
    let config = lazybox_config::Config::load().map_err(std::io::Error::other)?;
    let relay = relay_from_config(
        &config.relay.entitlement,
        std::env::var("LAZYBOX_PLATFORM_API_KEY").ok(),
    )?;
    let listener = TcpListener::bind(&bind).await?;
    tracing::info!(%bind, "lazybox-relay listening");
    Arc::new(relay).serve(listener).await
}

fn relay_from_config(
    config: &RelayEntitlementConfig,
    env_api_key: Option<String>,
) -> std::io::Result<Relay> {
    let platform_url = config
        .platform_url
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty());
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
            "relay.entitlement.platform_url requires relay.entitlement.api_key or LAZYBOX_PLATFORM_API_KEY",
        )),
        (None, Some(_)) => Err(std::io::Error::other(
            "relay entitlement API key requires relay.entitlement.platform_url",
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn entitlement_config_is_optional() {
        assert!(relay_from_config(&RelayEntitlementConfig::default(), None).is_ok());
    }

    #[test]
    fn entitlement_config_requires_url_and_key() {
        let only_url = RelayEntitlementConfig {
            platform_url: Some("https://platform.example".into()),
            api_key: None,
        };
        assert!(relay_from_config(&only_url, None).is_err());

        let only_key = RelayEntitlementConfig {
            platform_url: None,
            api_key: Some("secret".into()),
        };
        assert!(relay_from_config(&only_key, None).is_err());
    }

    #[test]
    fn environment_api_key_completes_platform_config() {
        let config = RelayEntitlementConfig {
            platform_url: Some("https://platform.example".into()),
            api_key: None,
        };
        assert!(relay_from_config(&config, Some("secret".into())).is_ok());
    }
}
