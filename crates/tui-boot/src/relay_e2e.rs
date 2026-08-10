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

use lazybox_e2e_channel::{ChannelError, Identity, PublicKey, initiator_handshake};
use lazybox_ipc::socket::Redial;
use lazybox_ipc::transport::{BoxRead, BoxWrite};

/// `<identity>/box_channel_key` — the box's persistent X25519 channel
/// identity (private + public, hex).
const CHANNEL_KEY_FILE: &str = "box_channel_key";

/// Load the box's channel identity from `dir`, generating and persisting
/// a fresh one on first run. The private key is written mode `0600`,
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

    match write_new_owner_only(&path, contents.as_bytes()) {
        Ok(()) => Ok(identity),
        // A concurrent first-run already published a complete key — adopt
        // it rather than clobber, so both processes pin the same box.
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => read_channel_identity(&path)?
            .ok_or_else(|| {
                anyhow::anyhow!("channel key at {} vanished after write", path.display())
            }),
        Err(e) => Err(anyhow::anyhow!("write {}: {e}", path.display())),
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
            let tcp = lazybox_relay::connect_through_relay(&relay_addr, &box_id).await?;
            // A device identity the box learns during the handshake. Step 1
            // does not yet bind it to a per-device credential (that is the
            // pairing work), so a fresh ephemeral key per dial is correct.
            let device = Identity::generate().map_err(channel_io)?;
            let encrypted = initiator_handshake(tcp, &device, &box_key)
                .await
                .map_err(channel_io)?;
            let (rd, wr) = tokio::io::split(encrypted);
            Ok((Box::new(rd) as BoxRead, Box::new(wr) as BoxWrite))
        })
    })
}

fn channel_io(e: ChannelError) -> io::Error {
    io::Error::other(e.to_string())
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
    hex::decode(line.trim()).map_err(|_| {
        anyhow::anyhow!(
            "channel key at {} has a malformed {which} key",
            path.display()
        )
    })
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
}
