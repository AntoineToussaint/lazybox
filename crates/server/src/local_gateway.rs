//! Published loopback API gateway: the process that owns the daemon
//! also serves the JSON `/v1` gateway on an ephemeral loopback port and
//! publishes `{pid, url, token}` to `<runtime>/gateway.json` (0600), so
//! a second front-end — the desktop launched while the TUI is running,
//! or a second desktop — **attaches** to the live daemon instead of
//! refusing to start ("daemon is already owned by process N; stop it").
//!
//! Discovery is only trusted when its `pid` matches the live pid-file
//! owner, so a crashed run's leftover file can never point an attach at
//! the wrong process. The file is removed on orderly shutdown and
//! rewritten by the next owner.

use crate::ServerConfig;
use crate::api_gateway::{GatewayOptions, serve_listener_until};
use crate::lifecycle;
use std::io::Write;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::PathBuf;

/// What an attaching front-end needs: where the owner's gateway
/// listens and the bearer that guards it.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
pub struct GatewayDiscovery {
    pub pid: u32,
    pub url: String,
    pub token: String,
}

pub fn discovery_path() -> PathBuf {
    lifecycle::runtime_dir().join("gateway.json")
}

/// The published gateway of the CURRENT daemon owner, or `None` when no
/// daemon runs, the file is missing/garbled, or it was written by a
/// process that no longer owns the pid file (crash leftovers).
pub fn read_discovery() -> Option<GatewayDiscovery> {
    let raw = std::fs::read_to_string(discovery_path()).ok()?;
    let discovery: GatewayDiscovery = serde_json::from_str(&raw).ok()?;
    let owner = lifecycle::read_pid(&lifecycle::pid_path()).ok().flatten()?;
    (owner == discovery.pid).then_some(discovery)
}

/// Atomic 0600 write — the token guards agent control, so the file gets
/// the same posture as the daemon socket's directory.
fn write_discovery(discovery: &GatewayDiscovery) -> std::io::Result<()> {
    lifecycle::ensure_runtime_dir()?;
    let path = discovery_path();
    let tmp = path.with_extension("json.tmp");
    {
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(&tmp)?;
        file.write_all(serde_json::to_string(discovery)?.as_bytes())?;
        file.sync_all()?;
    }
    std::fs::rename(&tmp, &path)
}

pub fn remove_discovery() {
    let _ = std::fs::remove_file(discovery_path());
}

/// Publish discovery for a gateway the caller binds and serves itself
/// (the desktop's embedded owner path). `pid` is the current process.
pub fn publish_discovery(url: &str, token: &str) -> std::io::Result<()> {
    write_discovery(&GatewayDiscovery {
        pid: std::process::id(),
        url: url.to_string(),
        token: token.to_string(),
    })
}

/// A running published gateway. Call [`Self::shutdown`] on the quit
/// path — it drains the gateway and removes the discovery file.
pub struct PublishedGateway {
    pub url: String,
    pub token: String,
    shutdown: tokio::sync::watch::Sender<bool>,
    task: tokio::task::JoinHandle<()>,
}

impl PublishedGateway {
    pub async fn shutdown(self) {
        let _ = self.shutdown.send(true);
        let _ = self.task.await;
        remove_discovery();
    }
}

/// Bind an ephemeral loopback port, publish discovery, and serve the
/// JSON gateway against `config` until shutdown. Bearer-token auth is
/// always on (a fresh UUID per run); the listener never leaves
/// 127.0.0.1.
pub async fn publish_local_gateway(config: ServerConfig) -> Result<PublishedGateway, String> {
    let listener =
        tokio::net::TcpListener::bind(SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0))
            .await
            .map_err(|error| format!("bind published local gateway: {error}"))?;
    let address = listener
        .local_addr()
        .map_err(|error| format!("read published gateway address: {error}"))?;
    let token = uuid::Uuid::new_v4().simple().to_string();
    let discovery = GatewayDiscovery {
        pid: std::process::id(),
        url: format!("http://{address}"),
        token: token.clone(),
    };
    write_discovery(&discovery).map_err(|error| format!("publish gateway discovery: {error}"))?;

    let options = GatewayOptions {
        bind_addr: address,
        bearer_token: Some(token.clone()),
        ..GatewayOptions::default()
    };
    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
    let task = tokio::spawn(async move {
        if let Err(error) = serve_listener_until(
            config,
            options,
            listener,
            shutdown_rx,
            crate::MUTATION_DRAIN_TIMEOUT + std::time::Duration::from_secs(1),
        )
        .await
        {
            tracing::error!("published local gateway stopped: {error}");
        }
    });
    tracing::info!(url = %discovery.url, "published local API gateway for attaching front-ends");
    Ok(PublishedGateway {
        url: discovery.url,
        token,
        shutdown: shutdown_tx,
        task,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Serializes the two `LAZYBOX_HOME` mutators below. No other
    /// server lib test touches the runtime dir, so a module-local lock
    /// is sufficient isolation.
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// Discovery round-trips only while its pid owns the pid file; a
    /// mismatched or missing owner invalidates it (crash leftovers must
    /// never point an attach at the wrong process).
    #[test]
    fn discovery_is_gated_on_the_live_pid_owner() {
        let _env = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let home = std::env::temp_dir().join(format!("lazybox-gwdisc-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        std::fs::create_dir_all(&home).unwrap();
        // SAFETY: test-scoped env mutation, serialized by the harness's
        // process-level env lock convention (single-threaded here).
        unsafe { std::env::set_var("LAZYBOX_HOME", &home) };

        let discovery = GatewayDiscovery {
            pid: std::process::id(),
            url: "http://127.0.0.1:1".into(),
            token: "t".into(),
        };
        write_discovery(&discovery).expect("write discovery");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(discovery_path())
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600, "token file must be private");
        }

        // No pid file → no trusted discovery.
        assert_eq!(read_discovery(), None);

        // Our pid owns the pid file → discovery is trusted.
        lifecycle::ensure_runtime_dir().unwrap();
        std::fs::write(lifecycle::pid_path(), std::process::id().to_string()).unwrap();
        assert_eq!(read_discovery(), Some(discovery.clone()));

        // A different (live) owner → the file is stale, not trusted.
        std::fs::write(lifecycle::pid_path(), "1").unwrap();
        assert_eq!(read_discovery(), None);

        remove_discovery();
        std::fs::write(lifecycle::pid_path(), std::process::id().to_string()).unwrap();
        assert_eq!(read_discovery(), None, "removed file stays gone");

        unsafe { std::env::remove_var("LAZYBOX_HOME") };
        let _ = std::fs::remove_dir_all(&home);
    }

    /// Publishing binds a loopback port, writes trusted discovery, and
    /// shutdown removes it again.
    // The guard deliberately spans the awaits: it serializes the
    // LAZYBOX_HOME mutation for the whole test body, and the
    // current-thread test runtime can't deadlock on it.
    #[allow(clippy::await_holding_lock)]
    #[tokio::test]
    async fn publish_writes_and_shutdown_removes_discovery() {
        let _env = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let home = std::env::temp_dir().join(format!("lazybox-gwpub-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        std::fs::create_dir_all(&home).unwrap();
        // SAFETY: see above.
        unsafe { std::env::set_var("LAZYBOX_HOME", &home) };
        lifecycle::ensure_runtime_dir().unwrap();
        std::fs::write(lifecycle::pid_path(), std::process::id().to_string()).unwrap();

        let config = crate::ServerConfig::in_memory();
        let gateway = publish_local_gateway(config).await.expect("publish");
        let discovery = read_discovery().expect("discovery trusted while owner lives");
        assert_eq!(discovery.url, gateway.url);
        assert_eq!(discovery.token, gateway.token);
        assert!(discovery.url.starts_with("http://127.0.0.1:"));

        gateway.shutdown().await;
        assert_eq!(read_discovery(), None, "shutdown unpublishes");

        unsafe { std::env::remove_var("LAZYBOX_HOME") };
        let _ = std::fs::remove_dir_all(&home);
    }
}
