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

    let listener = TcpListener::bind(&listen_addr).await?;
    tracing::info!(%listen_addr, "lazybox-relay listening");
    Arc::new(Relay::new()).serve(listener).await
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
