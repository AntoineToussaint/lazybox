//! Journey: a second front-end attaches to a running daemon owner's
//! published gateway — the desktop-launched-while-the-TUI-runs flow.
//! The owner publishes {pid, url, token}; the attacher reads discovery
//! and drives the JSON `/v1` gateway over real TCP with the bearer.

use std::sync::Mutex;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

static ENV_LOCK: Mutex<()> = Mutex::new(());

struct IsolatedHome {
    _guard: std::sync::MutexGuard<'static, ()>,
    _tmp: tempfile::TempDir,
    prev: Option<std::ffi::OsString>,
}

impl IsolatedHome {
    fn new() -> Self {
        let guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let tmp = tempfile::TempDir::new().unwrap();
        let prev = std::env::var_os("LAZYBOX_HOME");
        // SAFETY: integration tests run one binary per file and this
        // file serializes its cases with ENV_LOCK.
        unsafe { std::env::set_var("LAZYBOX_HOME", tmp.path()) };
        Self {
            _guard: guard,
            _tmp: tmp,
            prev,
        }
    }
}

impl Drop for IsolatedHome {
    fn drop(&mut self) {
        match &self.prev {
            // SAFETY: see above.
            Some(value) => unsafe { std::env::set_var("LAZYBOX_HOME", value) },
            None => unsafe { std::env::remove_var("LAZYBOX_HOME") },
        }
    }
}

async fn http_get(url: &str, path: &str, bearer: Option<&str>) -> (u16, String) {
    let address = url.strip_prefix("http://").expect("loopback http url");
    let mut stream = tokio::net::TcpStream::connect(address)
        .await
        .expect("connect published gateway");
    let auth = bearer
        .map(|token| format!("Authorization: Bearer {token}\r\n"))
        .unwrap_or_default();
    let request =
        format!("GET {path} HTTP/1.1\r\nHost: {address}\r\n{auth}Connection: close\r\n\r\n");
    stream.write_all(request.as_bytes()).await.expect("send");
    let mut response = Vec::new();
    stream.read_to_end(&mut response).await.expect("read");
    let response = String::from_utf8_lossy(&response).to_string();
    let status = response
        .split_whitespace()
        .nth(1)
        .and_then(|code| code.parse().ok())
        .expect("status line");
    (status, response)
}

/// The full attach journey: owner publishes, attacher discovers and is
/// admitted with the bearer (and refused without), and the owner's
/// shutdown unpublishes so a late attacher can't latch onto a draining
/// daemon.
#[tokio::test]
async fn second_front_end_attaches_to_the_published_gateway() {
    let _home = IsolatedHome::new();
    // The "owner" (a TUI's embedded daemon in production): pid file +
    // published gateway over an in-memory daemon.
    lazybox_server::lifecycle::ensure_runtime_dir().unwrap();
    std::fs::write(
        lazybox_server::lifecycle::pid_path(),
        std::process::id().to_string(),
    )
    .unwrap();
    let config = lazybox_server::ServerConfig::in_memory();
    let gateway = lazybox_server::local_gateway::publish_local_gateway(config)
        .await
        .expect("publish");

    // The "desktop": discover, then drive /v1 over real TCP.
    let discovery =
        lazybox_server::local_gateway::read_discovery().expect("discovery names the owner");
    assert_eq!(discovery.pid, std::process::id());

    let (status, body) = http_get(&discovery.url, "/v1/health", Some(&discovery.token)).await;
    assert_eq!(status, 200, "authed attach reaches the daemon: {body}");

    let (status, _) = http_get(&discovery.url, "/v1/health", None).await;
    assert_eq!(status, 401, "the bearer is required — no open local port");

    gateway.shutdown().await;
    assert!(
        lazybox_server::local_gateway::read_discovery().is_none(),
        "shutdown unpublishes discovery",
    );
}
