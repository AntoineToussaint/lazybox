//! The rendezvous relay service — a separate, codefly-hosted deployable.
//!
//! ```text
//! lazybox-relay [BIND_ADDR]      default 0.0.0.0:9443
//! ```
//!
//! Boxes dial out and register; clients reach them by box-id; the relay
//! forwards ciphertext only and executes nothing.

use std::sync::Arc;

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
    let listener = TcpListener::bind(&bind).await?;
    tracing::info!(%bind, "lazybox-relay listening");
    Arc::new(Relay::new()).serve(listener).await
}
