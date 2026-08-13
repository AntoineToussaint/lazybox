//! The box's end-to-end channel identity and the client-side relay dial.
//!
//! The relay forwards ciphertext only; the encryption terminates at the
//! box (`serve`) and the connecting client. Both sides speak the Noise
//! `IK` channel from [`lazybox_e2e_channel`]: the box holds a persistent
//! X25519 identity whose public key the client pins, so a malicious relay
//! can forward but never MITM.
//!
//! The box's channel key is a **separate X25519 keypair** from the
//! Ed25519 `BoxIdentity` (`lazybox-identity`): Noise `IK` is an X25519
//! protocol and the two curves are not interchangeable, so the box
//! persists a dedicated channel key rather than converting the signing
//! key. Both live under the identity dir with the same owner-only care.

use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use lazybox_e2e_channel::{ChannelError, Identity, PublicKey, initiator_handshake};
use lazybox_ipc::socket::{Redial, RedialError};
use lazybox_ipc::transport::{BoxRead, BoxWrite};

/// `<identity>/box_channel_key` — the box's persistent X25519 channel
/// identity (private + public, hex).
const CHANNEL_KEY_FILE: &str = "box_channel_key";

/// Per-(process, call) disambiguator for the channel-key temp file, so
/// racing threads within one process don't collide on the same temp path.
static TMP_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Ceiling on reaching the box through the relay and completing the E2E
/// handshake. Bounds the dial so a box that registered but whose daemon
/// (or Noise responder) stalls surfaces as a retryable error instead of
/// hanging the launch / wedging the reconnect supervisor — matching the
/// bounded handshake every other connect path in the tree uses.
const RELAY_DIAL_TIMEOUT: Duration = Duration::from_secs(10);

/// Load the box's channel identity from `dir`, generating and persisting
/// a fresh one on first run. The private key is written mode `0600` and
/// published atomically so racing first-runs converge on one key (the
/// same discipline `BoxIdentity` uses for its seed).
pub fn load_or_generate_channel_identity(dir: &Path) -> anyhow::Result<Identity> {
    let path = dir.join(CHANNEL_KEY_FILE);
    if let Some(identity) = read_channel_identity(&path)? {
        return Ok(identity);
    }
    std::fs::create_dir_all(dir).map_err(|e| anyhow::anyhow!("create {}: {e}", dir.display()))?;

    let identity =
        Identity::generate().map_err(|e| anyhow::anyhow!("generate channel key: {e}"))?;
    let contents = format!(
        "{}\n{}\n",
        hex::encode(identity.private_key()),
        hex::encode(identity.public_key().as_bytes())
    );

    // Publish atomically: write the full contents to a private temp file,
    // then hard-link it into place. `link` fails `AlreadyExists` when a
    // racing first-run already created the key, so a loser adopts a
    // *complete* file rather than reading the empty window a bare
    // `create_new` exposes between file creation and the content write.
    // The temp name is unique per (process, call) so racing threads don't
    // collide on it.
    let seq = TMP_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let tmp_path = dir.join(format!(
        ".{CHANNEL_KEY_FILE}.{}.{seq}.tmp",
        std::process::id()
    ));
    write_new_owner_only(&tmp_path, contents.as_bytes())
        .map_err(|e| anyhow::anyhow!("write {}: {e}", tmp_path.display()))?;
    let link_result = std::fs::hard_link(&tmp_path, &path);
    let _ = std::fs::remove_file(&tmp_path);
    match link_result {
        Ok(()) => Ok(identity),
        // A concurrent first-run already published a complete key — adopt
        // it rather than clobber, so both processes pin the same box.
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => read_channel_identity(&path)?
            .ok_or_else(|| {
                anyhow::anyhow!("channel key at {} vanished after write", path.display())
            }),
        Err(e) => Err(anyhow::anyhow!("link {}: {e}", path.display())),
    }
}

/// Parse the `--box-key` a client pins: 64 hex chars → a 32-byte X25519
/// public key.
pub fn parse_box_key(hex_key: &str) -> anyhow::Result<PublicKey> {
    let bytes =
        hex::decode(hex_key.trim()).map_err(|_| anyhow::anyhow!("box key must be hex-encoded"))?;
    let bytes: [u8; 32] = bytes
        .try_into()
        .map_err(|_| anyhow::anyhow!("box key must be 32 bytes (64 hex chars)"))?;
    Ok(PublicKey::from_bytes(bytes))
}

/// A [`Redial`] that reaches the box `box_id` through the relay and wraps
/// the brokered stream in the E2E channel, pinning `box_key`. The IPC
/// client runs its own handshake and framing *inside* the returned
/// encrypted halves, so a relay flap reconnects transparently.
pub fn relay_redial(relay_addr: String, box_id: String, box_key: PublicKey) -> Redial {
    Arc::new(move || {
        let relay_addr = relay_addr.clone();
        let box_id = box_id.clone();
        Box::pin(async move {
            let dial = async {
                let tcp = lazybox_relay::connect_through_relay(&relay_addr, &box_id)
                    .await
                    .map_err(relay_redial_error)?;
                // A device identity the box learns during the handshake.
                // Step 1 does not yet bind it to a per-device credential
                // (that is the pairing work), so a fresh ephemeral key per
                // dial is correct.
                let device = Identity::generate()
                    .map_err(channel_io)
                    .map_err(RedialError::retryable)?;
                let encrypted = initiator_handshake(tcp, &device, &box_key)
                    .await
                    .map_err(channel_io)
                    .map_err(RedialError::retryable)?;
                let (rd, wr) = tokio::io::split(encrypted);
                Ok((Box::new(rd) as BoxRead, Box::new(wr) as BoxWrite))
            };
            match tokio::time::timeout(RELAY_DIAL_TIMEOUT, dial).await {
                Ok(result) => result,
                Err(_) => Err(RedialError::retryable(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "relay dial + E2E handshake timed out",
                ))),
            }
        })
    })
}

fn channel_io(e: ChannelError) -> io::Error {
    io::Error::other(e.to_string())
}

fn relay_redial_error(error: lazybox_relay::RelayClientError) -> RedialError {
    match error {
        error @ lazybox_relay::RelayClientError::SubscriptionRequired => {
            RedialError::terminal(io::Error::new(io::ErrorKind::PermissionDenied, error))
        }
        error @ lazybox_relay::RelayClientError::AuthenticationFailed => {
            RedialError::terminal(io::Error::new(io::ErrorKind::PermissionDenied, error))
        }
        lazybox_relay::RelayClientError::Io(error) => {
            RedialError::retryable(io::Error::new(error.kind(), error))
        }
        error @ lazybox_relay::RelayClientError::Unavailable { .. } => {
            RedialError::retryable(io::Error::new(io::ErrorKind::NotFound, error))
        }
    }
}

/// Read a persisted channel identity, or `None` if the file is absent.
fn read_channel_identity(path: &Path) -> anyhow::Result<Option<Identity>> {
    let contents = match std::fs::read_to_string(path) {
        Ok(contents) => contents,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(anyhow::anyhow!("read {}: {e}", path.display())),
    };
    let mut lines = contents.lines();
    let private = decode_key(lines.next(), path, "private")?;
    let public = decode_key(lines.next(), path, "public")?;
    Ok(Some(Identity::from_keypair(&private, &public)))
}

fn decode_key(line: Option<&str>, path: &Path, which: &str) -> anyhow::Result<Vec<u8>> {
    let line = line.ok_or_else(|| {
        anyhow::anyhow!(
            "channel key at {} is missing its {which} key",
            path.display()
        )
    })?;
    let bytes = hex::decode(line.trim()).map_err(|_| {
        anyhow::anyhow!(
            "channel key at {} has a malformed {which} key",
            path.display()
        )
    })?;
    // X25519 keys are 32 bytes. Reject a truncated/corrupt file here with a
    // clear error rather than letting it surface as a cryptic Noise failure
    // at connect time.
    if bytes.len() != 32 {
        anyhow::bail!(
            "channel key at {} has a {which} key of {} bytes, expected 32",
            path.display(),
            bytes.len()
        );
    }
    Ok(bytes)
}

/// Create `path` exclusively (`O_EXCL`, mode `0600`) and write `bytes`:
/// fails with `AlreadyExists` rather than truncating an existing key.
fn write_new_owner_only(path: &Path, bytes: &[u8]) -> io::Result<()> {
    use io::Write;
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(path)?;
    file.write_all(bytes)
}

/// Where the channel identity lives — the shared box identity dir.
pub fn channel_identity_dir() -> PathBuf {
    lazybox_core::paths::identity_dir()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn channel_identity_is_generated_then_stable() {
        let dir = tempfile::tempdir().unwrap();
        let first = load_or_generate_channel_identity(dir.path()).unwrap();
        let pubkey = first.public_key();
        let second = load_or_generate_channel_identity(dir.path()).unwrap();
        assert_eq!(
            second.public_key(),
            pubkey,
            "the channel key must persist across calls"
        );
    }

    #[test]
    fn concurrent_first_run_converges_on_one_channel_key() {
        // Many threads hitting a fresh box at once must all end up with the
        // same persisted key; the temp-file + hard-link publish is what
        // guarantees it. A bare create_new + write would let a loser read
        // the empty window and fail, or diverge.
        let dir = tempfile::tempdir().unwrap();
        let path = Arc::new(dir.path().to_path_buf());
        let keys: Vec<[u8; 32]> = (0..8)
            .map(|_| {
                let path = Arc::clone(&path);
                std::thread::spawn(move || {
                    *load_or_generate_channel_identity(&path)
                        .unwrap()
                        .public_key()
                        .as_bytes()
                })
            })
            .collect::<Vec<_>>()
            .into_iter()
            .map(|handle| handle.join().unwrap())
            .collect();
        assert!(
            keys.iter().all(|k| *k == keys[0]),
            "racing first-runs must adopt one channel key",
        );
    }

    #[test]
    fn malformed_channel_key_is_rejected_on_load() {
        // A truncated / corrupt key file must fail at load with a clear
        // error, not silently produce an Identity that blows up at
        // handshake time.
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join(CHANNEL_KEY_FILE),
            "deadbeef\ndeadbeef\n", // 4 bytes each, not 32
        )
        .unwrap();
        assert!(load_or_generate_channel_identity(dir.path()).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn private_channel_key_is_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        load_or_generate_channel_identity(dir.path()).unwrap();
        let mode = std::fs::metadata(dir.path().join(CHANNEL_KEY_FILE))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
    }

    #[test]
    fn parse_box_key_round_trips_the_public_key() {
        let identity = Identity::generate().unwrap();
        let hex_key = hex::encode(identity.public_key().as_bytes());
        assert_eq!(parse_box_key(&hex_key).unwrap(), identity.public_key());
    }

    #[test]
    fn parse_box_key_rejects_garbage() {
        assert!(parse_box_key("not-hex").is_err());
        assert!(parse_box_key("dead").is_err(), "wrong length is rejected");
    }

    #[test]
    fn subscription_denial_keeps_its_typed_kind_and_message() {
        let error = relay_redial_error(lazybox_relay::RelayClientError::SubscriptionRequired);
        let RedialError::Terminal(error) = error else {
            panic!("subscription denial must stop reconnecting");
        };
        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
        assert_eq!(
            error.to_string(),
            lazybox_relay::SUBSCRIPTION_REQUIRED_MESSAGE
        );
    }
}
