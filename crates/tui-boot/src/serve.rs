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

/// `lazybox serve --relay <host:port> [--account <id>] [--box-id <id>]
/// [--socket <path>] [--insecure-no-auth]`.
///
/// `--relay` (or `LAZYBOX_RELAY`) is required; `--account` (or
/// `LAZYBOX_ACCOUNT`) defaults to `self-hosted` — the relay checks it
/// against the entitlement gate before brokering. The channel is
/// encrypted by default; `--insecure-no-auth` is a loopback-testing
/// escape hatch that forwards plaintext.
pub async fn serve_subcommand(args: &[String]) -> anyhow::Result<()> {
    let mut args = args.to_vec();
    let insecure_no_auth = take_flag(&mut args, "--insecure-no-auth");
    let relay_addr = take_value(&mut args, "--relay")
        .or_else(|| std::env::var("LAZYBOX_RELAY").ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    let Some(relay_addr) = relay_addr else {
        anyhow::bail!("lazybox serve needs a relay: pass --relay <host:port> or set LAZYBOX_RELAY",);
    };
    let account = take_value(&mut args, "--account")
        .or_else(|| std::env::var("LAZYBOX_ACCOUNT").ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "self-hosted".to_string());
    let socket_path = take_value(&mut args, "--socket")
        .map(PathBuf::from)
        .unwrap_or_else(lifecycle::socket_path);
    let box_id = match take_value(&mut args, "--box-id") {
        Some(id) => id.trim().to_string(),
        None => load_or_create_box_id(&box_id_file())?,
    };

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
    println!("dialing relay {relay_addr} (account {account})");

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
            account.clone(),
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
    let mut daemon = match UnixStream::connect(daemon_socket).await {
        Ok(daemon) => daemon,
        Err(error) => {
            tracing::warn!(
                %error,
                socket = %daemon_socket.display(),
                "brokered client could not reach the local daemon",
            );
            return;
        }
    };
    match channel_identity {
        Some(identity) => match responder_handshake(relay_stream, identity).await {
            // The device's public key is learned here; binding it to a
            // per-device credential is the pairing work (#980 step 2).
            Ok((mut encrypted, _device_key)) => {
                let _ = copy_bidirectional(&mut encrypted, &mut daemon).await;
            }
            Err(error) => {
                tracing::warn!(%error, "E2E handshake with brokered client failed");
            }
        },
        None => {
            let _ = copy_bidirectional(&mut relay_stream, &mut daemon).await;
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
}
