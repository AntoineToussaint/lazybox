//! The box's persistent identity keypair.
//!
//! A lazybox daemon ("box") owns one long-lived Ed25519 keypair,
//! generated on first run and persisted to disk. Its public key is the
//! box's stable identity: a pairing QR pins it so a malicious relay
//! cannot impersonate the box, and remote clients verify signatures
//! against it.

use std::io;
use std::path::{Path, PathBuf};

use ed25519_dalek::{Signature, Signer, SigningKey, Verifier, VerifyingKey};
use sha2::{Digest, Sha256};

const PRIVATE_KEY_FILE: &str = "box_ed25519";
const PUBLIC_KEY_FILE: &str = "box_ed25519.pub";

/// Per-(process, call) disambiguator for the seed temp file, so racing
/// threads within one process don't collide on the same temp path.
static TMP_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

#[derive(Debug, thiserror::Error)]
pub enum BoxIdentityError {
    #[error("box identity io at `{path}`: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("box identity key material at `{path}` is malformed")]
    Malformed { path: PathBuf },
}

/// A loaded box identity. Cheap to clone; wraps the signing key.
pub struct BoxIdentity {
    signing_key: SigningKey,
}

impl BoxIdentity {
    /// Load the keypair from `dir`, generating and persisting a fresh
    /// one on first run. The private seed is written mode `0600`.
    pub fn load_or_generate(dir: impl AsRef<Path>) -> Result<Self, BoxIdentityError> {
        let dir = dir.as_ref();
        let private_path = dir.join(PRIVATE_KEY_FILE);
        if private_path.exists() {
            return Self::load(&private_path);
        }
        std::fs::create_dir_all(dir).map_err(|source| BoxIdentityError::Io {
            path: dir.to_path_buf(),
            source,
        })?;
        let signing_key = SigningKey::generate(&mut rand::rng());
        let seed_hex = hex::encode(signing_key.to_bytes());

        // Publish the seed atomically: write the full contents to a private
        // temp file, then hard-link it into place. `link` fails with
        // `AlreadyExists` when another first-run already created the seed,
        // so a loser adopts a *complete* file rather than clobbering it or
        // reading the empty window a bare `create_new` would expose between
        // file creation and the content write. The temp name is unique per
        // (process, call) so racing threads/processes don't collide on it.
        let seq = TMP_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let tmp_path = dir.join(format!(
            ".{PRIVATE_KEY_FILE}.{}.{seq}.tmp",
            std::process::id()
        ));
        write_new_owner_only(&tmp_path, seed_hex.as_bytes()).map_err(|source| {
            BoxIdentityError::Io {
                path: tmp_path.clone(),
                source,
            }
        })?;
        let link_result = std::fs::hard_link(&tmp_path, &private_path);
        let _ = std::fs::remove_file(&tmp_path);
        match link_result {
            Ok(()) => {}
            Err(source) if source.kind() == io::ErrorKind::AlreadyExists => {
                return Self::load(&private_path);
            }
            Err(source) => {
                return Err(BoxIdentityError::Io {
                    path: private_path.clone(),
                    source,
                });
            }
        }
        let public_path = dir.join(PUBLIC_KEY_FILE);
        let public_hex = hex::encode(signing_key.verifying_key().to_bytes());
        std::fs::write(&public_path, format!("{public_hex}\n")).map_err(|source| {
            BoxIdentityError::Io {
                path: public_path,
                source,
            }
        })?;
        Ok(Self { signing_key })
    }

    fn load(private_path: &Path) -> Result<Self, BoxIdentityError> {
        let contents =
            std::fs::read_to_string(private_path).map_err(|source| BoxIdentityError::Io {
                path: private_path.to_path_buf(),
                source,
            })?;
        let seed = hex::decode(contents.trim()).map_err(|_| BoxIdentityError::Malformed {
            path: private_path.to_path_buf(),
        })?;
        let seed: [u8; 32] = seed.try_into().map_err(|_| BoxIdentityError::Malformed {
            path: private_path.to_path_buf(),
        })?;
        Ok(Self {
            signing_key: SigningKey::from_bytes(&seed),
        })
    }

    /// The box's public key, hex-encoded (64 chars). This is what a
    /// pairing QR carries so clients can pin the box's identity.
    pub fn public_key_hex(&self) -> String {
        hex::encode(self.signing_key.verifying_key().to_bytes())
    }

    /// A short, stable fingerprint of the public key (32 hex chars =
    /// 128 bits) — a human-presentable box id.
    pub fn box_id(&self) -> String {
        let digest = Sha256::digest(self.signing_key.verifying_key().to_bytes());
        hex::encode(&digest[..16])
    }

    pub fn sign(&self, message: &[u8]) -> Signature {
        self.signing_key.sign(message)
    }

    pub fn verifying_key(&self) -> VerifyingKey {
        self.signing_key.verifying_key()
    }
}

/// Verify a signature against a hex-encoded box public key. Returns
/// `false` on any malformed input rather than erroring — a caller
/// checking authenticity only cares whether the signature holds.
pub fn verify(public_key_hex: &str, message: &[u8], signature: &Signature) -> bool {
    let Ok(bytes) = hex::decode(public_key_hex) else {
        return false;
    };
    let Ok(bytes) = <[u8; 32]>::try_from(bytes) else {
        return false;
    };
    let Ok(key) = VerifyingKey::from_bytes(&bytes) else {
        return false;
    };
    key.verify(message, signature).is_ok()
}

/// Create a new file exclusively (`O_EXCL`, mode `0600`) and write
/// `bytes`: fails with `AlreadyExists` rather than truncating an
/// existing file.
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generates_then_reloads_the_same_key() {
        let dir = tempfile::tempdir().unwrap();
        let first = BoxIdentity::load_or_generate(dir.path()).unwrap();
        let id = first.box_id();
        let pubkey = first.public_key_hex();

        // A second load reuses the persisted key, not a fresh one.
        let second = BoxIdentity::load_or_generate(dir.path()).unwrap();
        assert_eq!(second.box_id(), id);
        assert_eq!(second.public_key_hex(), pubkey);
        assert_eq!(pubkey.len(), 64);
        assert_eq!(id.len(), 32);
    }

    #[test]
    fn concurrent_first_run_converges_on_one_key() {
        use std::sync::Arc;
        // Many processes hitting a fresh box at once must all end up with the
        // same persisted key; the `create_new` + reload-on-conflict path is
        // what guarantees it (a truncating write would let them diverge).
        let dir = tempfile::tempdir().unwrap();
        let path = Arc::new(dir.path().to_path_buf());
        let ids: Vec<String> = (0..8)
            .map(|_| {
                let path = Arc::clone(&path);
                std::thread::spawn(move || BoxIdentity::load_or_generate(&*path).unwrap().box_id())
            })
            .collect::<Vec<_>>()
            .into_iter()
            .map(|handle| handle.join().unwrap())
            .collect();
        assert!(
            ids.iter().all(|id| *id == ids[0]),
            "racing first-runs must adopt one key, got {ids:?}"
        );
    }

    #[test]
    fn signatures_verify_against_the_public_key() {
        let dir = tempfile::tempdir().unwrap();
        let identity = BoxIdentity::load_or_generate(dir.path()).unwrap();
        let signature = identity.sign(b"pair this device");
        assert!(verify(
            &identity.public_key_hex(),
            b"pair this device",
            &signature
        ));
        assert!(!verify(&identity.public_key_hex(), b"tampered", &signature));
    }

    #[cfg(unix)]
    #[test]
    fn private_seed_is_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        BoxIdentity::load_or_generate(dir.path()).unwrap();
        let mode = std::fs::metadata(dir.path().join(PRIVATE_KEY_FILE))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
    }

    #[test]
    fn malformed_key_material_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(PRIVATE_KEY_FILE), "not-hex").unwrap();
        assert!(matches!(
            BoxIdentity::load_or_generate(dir.path()),
            Err(BoxIdentityError::Malformed { .. })
        ));
    }
}
