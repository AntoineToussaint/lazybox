//! `lazybox serve` — dial **out** to a rendezvous relay and expose this
//! box's local daemon to clients brokered through it.
//!
//! The box holds a control connection to a codefly-hosted relay
//! ([`lazybox_relay`]); for each client the relay brokers, we dial a
//! fresh data connection, terminate the end-to-end Noise channel on it,
//! and bridge the *decrypted* stream to the local daemon's Unix socket.
//! The relay forwards ciphertext only and executes nothing.
//!
//! The channel is **secure by default**: each brokered stream is wrapped
//! in the E2E responder ([`lazybox_e2e_channel`]) before the daemon sees
//! a byte, pinned to the box's persistent X25519 channel key
//! (`crate::relay_e2e`). `--insecure-no-auth` drops the encryption for
//! loopback testing only.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use lazybox_e2e_channel::{Identity, responder_handshake};
use tokio::io::copy_bidirectional;
use tokio::net::{TcpStream, UnixStream};

use crate::relay_e2e;
use crate::{lifecycle, take_flag, take_value};

/// Backoff between control-connection reconnect attempts.
const RECONNECT_DELAY: Duration = Duration::from_secs(3);

/// Ceiling on the E2E handshake with a brokered client. A conforming
/// client completes it in one round trip; the bound stops a client that
/// finishes the relay splice but then stalls the Noise handshake from
/// pinning a task (and a daemon connection) forever — the same discipline
/// the relay's own `handshake_timeout` enforces on its protocol frame.
const E2E_HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Debug)]
struct ServeOptions {
    relay_addr: String,
    socket_path: PathBuf,
    box_id: Option<String>,
    insecure_no_auth: bool,
}

/// `lazybox serve --relay <host:port> [--box-id <id>]
/// [--socket <path>] [--insecure-no-auth]`.
///
/// `--relay` (or `LAZYBOX_RELAY`) is required. The box's persistent
/// Ed25519 public key identifies its subscription to a hosted relay. The
/// channel is encrypted by default; `--insecure-no-auth` is a
/// loopback-testing escape hatch that forwards plaintext.
pub async fn serve_subcommand(args: &[String]) -> anyhow::Result<()> {
    let options = parse_serve_options(args, std::env::var("LAZYBOX_RELAY").ok())?;
    let ServeOptions {
        relay_addr,
        socket_path,
        box_id,
        insecure_no_auth,
    } = options;
    let box_id = match box_id {
        Some(id) => id,
        None => load_or_create_box_id(&box_id_file())?,
    };
    let box_identity = Arc::new(lazybox_identity::BoxIdentity::load_or_generate(
        lazybox_core::paths::identity_dir(),
    )?);

    serve(
        relay_addr,
        socket_path,
        box_id,
        box_identity,
        insecure_no_auth,
    )
    .await
}

fn parse_serve_options(
    args: &[String],
    relay_from_env: Option<String>,
) -> anyhow::Result<ServeOptions> {
    let mut args = args.to_vec();
    let insecure_no_auth = take_flag(&mut args, "--insecure-no-auth");
    let relay_addr = take_value(&mut args, "--relay")
        .or(relay_from_env)
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    let Some(relay_addr) = relay_addr else {
        anyhow::bail!("lazybox serve needs a relay: pass --relay <host:port> or set LAZYBOX_RELAY",);
    };
    let socket_path = take_value(&mut args, "--socket")
        .map(PathBuf::from)
        .unwrap_or_else(lifecycle::socket_path);
    let box_id = take_value(&mut args, "--box-id").map(|id| id.trim().to_string());
    if let Some(argument) = args.first() {
        anyhow::bail!("unrecognized `lazybox serve` argument: {argument}");
    }
    Ok(ServeOptions {
        relay_addr,
        socket_path,
        box_id,
        insecure_no_auth,
    })
}

async fn serve(
    relay_addr: String,
    socket_path: PathBuf,
    box_id: String,
    box_identity: Arc<lazybox_identity::BoxIdentity>,
    insecure_no_auth: bool,
) -> anyhow::Result<()> {
    // Secure by default: load (or generate on first run) the box's
    // persistent X25519 channel identity and terminate the Noise channel
    // on every brokered stream. `--insecure-no-auth` forwards plaintext —
    // a loopback-testing escape hatch only.
    let channel_identity = if insecure_no_auth {
        None
    } else {
        Some(Arc::new(relay_e2e::load_or_generate_channel_identity(
            &relay_e2e::channel_identity_dir(),
        )?))
    };

    if !socket_path.exists() {
        tracing::warn!(
            socket = %socket_path.display(),
            "no daemon socket yet — brokered clients will fail until `lazybox server start` runs",
        );
    }

    println!("lazybox serve: box-id {box_id}");
    match &channel_identity {
        Some(identity) => {
            println!(
                "box channel key {} (clients pin this with --box-key)",
                hex::encode(identity.public_key().as_bytes())
            );
        }
        None => println!(
            "WARNING: --insecure-no-auth: the relay path is unencrypted; anyone who reaches \
             the relay with box-id {box_id} can drive your daemon (loopback testing only)"
        ),
    }
    println!("dialing relay {relay_addr}");

    let bridge_socket = socket_path.clone();
    let on_client: lazybox_relay::OnClient = Arc::new(move |relay_stream| {
        let daemon_socket = bridge_socket.clone();
        let channel_identity = channel_identity.clone();
        Box::pin(async move {
            bridge_to_daemon(relay_stream, &daemon_socket, channel_identity.as_deref()).await;
        })
    });

    loop {
        match lazybox_relay::serve_box(
            relay_addr.clone(),
            box_id.clone(),
            Arc::clone(&box_identity),
            Arc::clone(&on_client),
        )
        .await
        {
            Ok(()) => tracing::warn!("relay control connection closed; reconnecting"),
            Err(error) => tracing::warn!(%error, "relay connection failed; retrying"),
        }
        tokio::time::sleep(RECONNECT_DELAY).await;
    }
}

/// Bridge a brokered relay stream to the local daemon socket. With a
/// channel identity, terminate the E2E responder first so the daemon only
/// ever sees decrypted frames and the relay only ever sees ciphertext;
/// without one (`--insecure-no-auth`) forward the bytes verbatim.
async fn bridge_to_daemon(
    mut relay_stream: TcpStream,
    daemon_socket: &Path,
    channel_identity: Option<&Identity>,
) {
    match channel_identity {
        Some(identity) => {
            // Terminate the E2E channel *before* touching the daemon: a
            // client that completes the relay splice but stalls the Noise
            // handshake must neither hang this task nor pin an open daemon
            // connection. Bound the handshake, and only dial the daemon
            // once it succeeds.
            let handshake = tokio::time::timeout(
                E2E_HANDSHAKE_TIMEOUT,
                responder_handshake(relay_stream, identity),
            )
            .await;
            // The device's public key is learned here; binding it to a
            // per-device credential is the pairing work (#980 step 2).
            let mut encrypted = match handshake {
                Ok(Ok((encrypted, _device_key))) => encrypted,
                Ok(Err(error)) => {
                    tracing::warn!(%error, "E2E handshake with brokered client failed");
                    return;
                }
                Err(_) => {
                    tracing::warn!("E2E handshake with brokered client timed out");
                    return;
                }
            };
            if let Some(mut daemon) = connect_daemon(daemon_socket).await {
                let _ = copy_bidirectional(&mut encrypted, &mut daemon).await;
            }
        }
        None => {
            if let Some(mut daemon) = connect_daemon(daemon_socket).await {
                let _ = copy_bidirectional(&mut relay_stream, &mut daemon).await;
            }
        }
    }
}

/// Dial the local daemon socket, logging (and yielding `None`) if it is
/// unreachable — e.g. no `lazybox server start` yet.
async fn connect_daemon(daemon_socket: &Path) -> Option<UnixStream> {
    match UnixStream::connect(daemon_socket).await {
        Ok(daemon) => Some(daemon),
        Err(error) => {
            tracing::warn!(
                %error,
                socket = %daemon_socket.display(),
                "brokered client could not reach the local daemon",
            );
            None
        }
    }
}

/// `<profile>/v2/box-id` — the persistent relay routing id.
fn box_id_file() -> PathBuf {
    lazybox_core::paths::state_root().join("box-id")
}

/// Read the persisted box-id, or generate + persist one on first run.
fn load_or_create_box_id(path: &Path) -> anyhow::Result<String> {
    if let Ok(existing) = std::fs::read_to_string(path) {
        let trimmed = existing.trim().to_string();
        if !trimmed.is_empty() {
            return Ok(trimmed);
        }
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| anyhow::anyhow!("create {}: {e}", parent.display()))?;
    }
    let box_id = uuid::Uuid::new_v4().simple().to_string();
    std::fs::write(path, format!("{box_id}\n"))
        .map_err(|e| anyhow::anyhow!("write {}: {e}", path.display()))?;
    Ok(box_id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn box_id_is_generated_then_stable() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("box-id");
        let first = load_or_create_box_id(&path).unwrap();
        assert!(!first.is_empty());
        let second = load_or_create_box_id(&path).unwrap();
        assert_eq!(first, second, "box-id must persist across calls");
    }

    #[test]
    fn blank_box_id_file_is_regenerated() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("box-id");
        std::fs::write(&path, "   \n").unwrap();
        let id = load_or_create_box_id(&path).unwrap();
        assert!(!id.is_empty());
    }

    #[test]
    fn removed_account_option_is_rejected_instead_of_ignored() {
        let args = vec![
            "--relay".to_string(),
            "relay.example:9443".to_string(),
            "--account".to_string(),
            "legacy-account".to_string(),
        ];

        let error = parse_serve_options(&args, None).unwrap_err();

        assert!(error.to_string().contains("--account"));
    }

    /// A brokered client whose E2E handshake fails must never cause the box
    /// to dial its daemon — the handshake terminates first. Guards the
    /// reorder that stops a probing / half-open client from pinning a
    /// daemon connection (and a task) before it has authenticated the box.
    #[tokio::test]
    async fn failed_handshake_never_dials_the_daemon() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicBool, Ordering};
        use tokio::net::{TcpListener, TcpStream, UnixListener};

        let dir = tempfile::tempdir().unwrap();
        let daemon_path = dir.path().join("daemon.sock");
        let listener = UnixListener::bind(&daemon_path).unwrap();
        let dialed = Arc::new(AtomicBool::new(false));
        let dialed_in_task = Arc::clone(&dialed);
        let accept = tokio::spawn(async move {
            if listener.accept().await.is_ok() {
                dialed_in_task.store(true, Ordering::SeqCst);
            }
        });

        // A brokered "client" that hangs up immediately, so the responder
        // handshake fails fast (EOF) rather than waiting out the timeout.
        let tcp = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = tcp.local_addr().unwrap();
        let client = TcpStream::connect(addr).await.unwrap();
        let (server, _peer) = tcp.accept().await.unwrap();
        drop(client);

        let identity = Identity::generate().unwrap();
        bridge_to_daemon(server, &daemon_path, Some(&identity)).await;

        // Give a (buggy) daemon-first dial a chance to land before asserting.
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(
            !dialed.load(Ordering::SeqCst),
            "a failed E2E handshake must not reach the daemon socket",
        );
        accept.abort();
    }
}
