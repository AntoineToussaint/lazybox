//! Revocable per-device credentials.
//!
//! The box mints one credential per paired device. Non-secret metadata
//! (id, name, principal, salted hash) is persisted as JSON so the box —
//! and a separate `lazybox device` CLI process — share a single source
//! of truth; the plaintext secret is held only in the [`DeviceKeystore`]
//! so it never lands in a backup-prone file. A device authenticates by
//! presenting its token; the box verifies against the stored hash.
//! Revoking a device flips one record and purges its keystore secret —
//! a lost phone revokes exactly one device, nothing else.

use std::path::PathBuf;
use std::sync::Mutex;

use chrono::{DateTime, Utc};
use rand::Rng;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::keystore::{DeviceKeystore, MemoryKeystore};

/// A device's principal id is `device:<id>`. This is what scopes the
/// device's provider credentials in the server's `CredentialStore`.
pub fn device_principal_id(device_id: &str) -> String {
    format!("device:{device_id}")
}

/// Persisted, non-secret record of one paired device.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DeviceRecord {
    pub id: String,
    pub name: String,
    pub principal_id: String,
    pub created_at: DateTime<Utc>,
    pub revoked_at: Option<DateTime<Utc>>,
    /// Hex `sha256(salt || secret)`. The plaintext secret is never
    /// stored here — only in the keystore.
    token_hash: String,
    salt: String,
}

impl DeviceRecord {
    pub fn is_revoked(&self) -> bool {
        self.revoked_at.is_some()
    }
}

/// The result of minting: the persisted record plus the one-time
/// plaintext token to deliver to the device. The token is `<id>.<secret>`.
#[derive(Debug, Clone)]
pub struct MintedDevice {
    pub record: DeviceRecord,
    pub token: String,
}

#[derive(Debug, thiserror::Error)]
pub enum DeviceRegistryError {
    #[error("device registry io at `{path}`: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("device registry at `{path}` is corrupt: {source}")]
    Corrupt {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },
    #[error("keystore error: {0}")]
    Keystore(#[from] crate::keystore::KeystoreError),
}

/// Where the registry's records live: a JSON file on disk (the box's
/// shared source of truth) or in-process memory (tests / ephemeral).
enum Backing {
    Disk(PathBuf),
    Memory(Mutex<Vec<DeviceRecord>>),
}

/// A registry of revocable per-device credentials.
pub struct DeviceRegistry {
    backing: Backing,
    keystore: Box<dyn DeviceKeystore>,
}

impl DeviceRegistry {
    /// A disk-backed registry rooted at `dir` (records in
    /// `<dir>/devices.json`, secrets in the given keystore). Records are
    /// re-read from disk on every operation so a running daemon sees
    /// devices minted by a separate `lazybox device` process.
    pub fn open(dir: impl Into<PathBuf>, keystore: Box<dyn DeviceKeystore>) -> Self {
        Self {
            backing: Backing::Disk(dir.into().join("devices.json")),
            keystore,
        }
    }

    /// A memory-backed registry with an in-memory keystore. Used by the
    /// default `ServerConfig` and by tests — it touches no real disk or
    /// OS keychain.
    pub fn ephemeral() -> Self {
        Self {
            backing: Backing::Memory(Mutex::new(Vec::new())),
            keystore: Box::new(MemoryKeystore::new()),
        }
    }

    /// Mint a credential for a new device. Returns the record and the
    /// one-time token; the caller must deliver the token to the device
    /// (it is not recoverable from the record afterwards).
    pub fn mint(&self, name: &str) -> Result<MintedDevice, DeviceRegistryError> {
        let id = random_hex(16);
        let secret = random_hex(32);
        let salt = random_hex(16);
        let record = DeviceRecord {
            principal_id: device_principal_id(&id),
            token_hash: hash_secret(&salt, &secret),
            salt,
            id: id.clone(),
            name: name.to_string(),
            created_at: Utc::now(),
            revoked_at: None,
        };

        let token = format!("{id}.{secret}");
        // Store the secret *before* persisting the record. A persisted
        // record authenticates off its hash alone (the keystore isn't
        // consulted on the auth path), so a record must never outlive a
        // failed keystore write — that would be a live device whose token
        // was never delivered to anyone. If the save then fails, roll the
        // secret back so neither store keeps an orphan.
        self.keystore.set(&id, &token)?;
        let mut records = self.load()?;
        records.push(record.clone());
        if let Err(error) = self.save(&records) {
            let _ = self.keystore.delete(&id);
            return Err(error);
        }
        Ok(MintedDevice { record, token })
    }

    /// Resolve a presented token to its principal id, or `None` if the
    /// token is unknown, malformed, or belongs to a revoked device.
    pub fn authenticate(&self, token: &str) -> Result<Option<String>, DeviceRegistryError> {
        let Some((id, secret)) = token.split_once('.') else {
            return Ok(None);
        };
        let records = self.load()?;
        let Some(record) = records.iter().find(|r| r.id == id && !r.is_revoked()) else {
            return Ok(None);
        };
        let candidate = hash_secret(&record.salt, secret);
        Ok(
            constant_time_eq(candidate.as_bytes(), record.token_hash.as_bytes())
                .then(|| record.principal_id.clone()),
        )
    }

    /// Revoke exactly one device by id and purge its keystore secret.
    /// Returns `true` if a live device was revoked, `false` if the id
    /// was unknown or already revoked.
    pub fn revoke(&self, id: &str) -> Result<bool, DeviceRegistryError> {
        let mut records = self.load()?;
        let Some(record) = records.iter_mut().find(|r| r.id == id && !r.is_revoked()) else {
            return Ok(false);
        };
        record.revoked_at = Some(Utc::now());
        self.save(&records)?;
        self.keystore.delete(id)?;
        Ok(true)
    }

    /// All device records, newest first.
    pub fn list(&self) -> Result<Vec<DeviceRecord>, DeviceRegistryError> {
        let mut records = self.load()?;
        records.sort_by_key(|record| std::cmp::Reverse(record.created_at));
        Ok(records)
    }

    /// The device's stored plaintext token, for re-presenting it via a
    /// pairing UI. `None` once the device is revoked (its secret is
    /// purged) or if the token was never persisted.
    pub fn token(&self, id: &str) -> Result<Option<String>, DeviceRegistryError> {
        match self.keystore.get(id) {
            Ok(token) => Ok(Some(token)),
            Err(crate::keystore::KeystoreError::NotFound(_)) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    fn load(&self) -> Result<Vec<DeviceRecord>, DeviceRegistryError> {
        match &self.backing {
            Backing::Memory(records) => {
                Ok(records.lock().unwrap_or_else(|e| e.into_inner()).clone())
            }
            Backing::Disk(path) => match std::fs::read(path) {
                Ok(bytes) => {
                    serde_json::from_slice(&bytes).map_err(|source| DeviceRegistryError::Corrupt {
                        path: path.clone(),
                        source,
                    })
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
                Err(source) => Err(DeviceRegistryError::Io {
                    path: path.clone(),
                    source,
                }),
            },
        }
    }

    fn save(&self, records: &[DeviceRecord]) -> Result<(), DeviceRegistryError> {
        match &self.backing {
            Backing::Memory(slot) => {
                *slot.lock().unwrap_or_else(|e| e.into_inner()) = records.to_vec();
                Ok(())
            }
            Backing::Disk(path) => {
                if let Some(parent) = path.parent() {
                    std::fs::create_dir_all(parent).map_err(|source| DeviceRegistryError::Io {
                        path: parent.to_path_buf(),
                        source,
                    })?;
                }
                let json = serde_json::to_vec_pretty(records).map_err(|source| {
                    DeviceRegistryError::Corrupt {
                        path: path.clone(),
                        source,
                    }
                })?;
                // Write via a temp file + rename so a crash mid-write
                // never leaves a truncated registry.
                let tmp = path.with_extension("json.tmp");
                std::fs::write(&tmp, &json).map_err(|source| DeviceRegistryError::Io {
                    path: tmp.clone(),
                    source,
                })?;
                std::fs::rename(&tmp, path).map_err(|source| DeviceRegistryError::Io {
                    path: path.clone(),
                    source,
                })
            }
        }
    }
}

fn random_hex(bytes: usize) -> String {
    let mut buf = vec![0u8; bytes];
    rand::rng().fill_bytes(&mut buf);
    hex::encode(buf)
}

fn hash_secret(salt: &str, secret: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(salt.as_bytes());
    hasher.update(secret.as_bytes());
    hex::encode(hasher.finalize())
}

/// Constant-time comparison so token verification doesn't leak how much
/// of a hash matched via response timing.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn disk_registry() -> (tempfile::TempDir, DeviceRegistry) {
        let dir = tempfile::tempdir().unwrap();
        let keystore = Box::new(MemoryKeystore::new());
        let registry = DeviceRegistry::open(dir.path(), keystore);
        (dir, registry)
    }

    /// A keystore whose `set` always fails — stands in for a locked
    /// Keychain or an unwritable file fallback.
    struct FailingKeystore;
    impl DeviceKeystore for FailingKeystore {
        fn set(&self, _key: &str, _secret: &str) -> Result<(), crate::keystore::KeystoreError> {
            Err(crate::keystore::KeystoreError::Backend(
                "keystore is sealed".into(),
            ))
        }
        fn get(&self, key: &str) -> Result<String, crate::keystore::KeystoreError> {
            Err(crate::keystore::KeystoreError::NotFound(key.into()))
        }
        fn delete(&self, _key: &str) -> Result<(), crate::keystore::KeystoreError> {
            Ok(())
        }
    }

    #[test]
    fn mint_persists_nothing_when_the_keystore_write_fails() {
        let dir = tempfile::tempdir().unwrap();
        let registry = DeviceRegistry::open(dir.path(), Box::new(FailingKeystore));

        // The keystore write fails, so mint must error and leave no record
        // behind — otherwise a phantom device would authenticate off its
        // hash while its token was never delivered to anyone.
        assert!(registry.mint("iPhone").is_err());
        assert!(registry.list().unwrap().is_empty());
        // The devices file, if written at all, holds no record.
        let devices = dir.path().join("devices.json");
        if devices.exists() {
            assert_eq!(std::fs::read_to_string(devices).unwrap().trim(), "[]");
        }
    }

    #[test]
    fn mint_then_authenticate_resolves_the_principal() {
        let (_dir, registry) = disk_registry();
        let minted = registry.mint("iPhone").unwrap();
        assert_eq!(
            minted.record.principal_id,
            format!("device:{}", minted.record.id)
        );
        assert!(minted.token.starts_with(&minted.record.id));

        let principal = registry.authenticate(&minted.token).unwrap();
        assert_eq!(principal, Some(minted.record.principal_id.clone()));
    }

    #[test]
    fn a_wrong_or_malformed_token_does_not_authenticate() {
        let (_dir, registry) = disk_registry();
        let minted = registry.mint("iPhone").unwrap();
        // Right device id, wrong secret.
        let forged = format!("{}.deadbeef", minted.record.id);
        assert_eq!(registry.authenticate(&forged).unwrap(), None);
        // No separator at all.
        assert_eq!(registry.authenticate("garbage").unwrap(), None);
        // Unknown device id.
        assert_eq!(registry.authenticate("unknown.secret").unwrap(), None);
    }

    #[test]
    fn revoking_one_device_leaves_the_others_working() {
        let (_dir, registry) = disk_registry();
        let phone = registry.mint("iPhone").unwrap();
        let laptop = registry.mint("Laptop").unwrap();

        assert!(registry.revoke(&phone.record.id).unwrap());

        // The revoked device can no longer authenticate...
        assert_eq!(registry.authenticate(&phone.token).unwrap(), None);
        // ...its secret is purged from the keystore...
        assert_eq!(registry.token(&phone.record.id).unwrap(), None);
        // ...but the other device is untouched.
        assert_eq!(
            registry.authenticate(&laptop.token).unwrap(),
            Some(laptop.record.principal_id.clone())
        );
        assert_eq!(
            registry.token(&laptop.record.id).unwrap(),
            Some(laptop.token)
        );
    }

    #[test]
    fn revoking_an_unknown_or_revoked_device_reports_no_change() {
        let (_dir, registry) = disk_registry();
        let phone = registry.mint("iPhone").unwrap();
        assert!(!registry.revoke("nope").unwrap());
        assert!(registry.revoke(&phone.record.id).unwrap());
        assert!(!registry.revoke(&phone.record.id).unwrap());
    }

    #[test]
    fn list_returns_records_newest_first_including_revoked() {
        let (_dir, registry) = disk_registry();
        let phone = registry.mint("iPhone").unwrap();
        registry.revoke(&phone.record.id).unwrap();
        registry.mint("Laptop").unwrap();

        let listed = registry.list().unwrap();
        assert_eq!(listed.len(), 2);
        assert_eq!(listed[0].name, "Laptop");
        assert_eq!(listed[1].name, "iPhone");
        assert!(listed[1].is_revoked());
    }

    #[test]
    fn records_persist_across_registry_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let token = {
            let registry = DeviceRegistry::open(dir.path(), Box::new(MemoryKeystore::new()));
            registry.mint("iPhone").unwrap().token
        };
        // A fresh registry over the same dir reads the persisted record.
        let reopened = DeviceRegistry::open(dir.path(), Box::new(MemoryKeystore::new()));
        assert_eq!(reopened.list().unwrap().len(), 1);
        assert!(reopened.authenticate(&token).unwrap().is_some());
    }

    #[test]
    fn secret_plaintext_is_not_written_to_the_json() {
        let dir = tempfile::tempdir().unwrap();
        let registry = DeviceRegistry::open(dir.path(), Box::new(MemoryKeystore::new()));
        let minted = registry.mint("iPhone").unwrap();
        let secret = minted.token.split_once('.').unwrap().1;
        let json = std::fs::read_to_string(dir.path().join("devices.json")).unwrap();
        assert!(!json.contains(secret));
    }

    #[test]
    fn ephemeral_registry_works_without_disk() {
        let registry = DeviceRegistry::ephemeral();
        let minted = registry.mint("iPhone").unwrap();
        assert_eq!(
            registry.authenticate(&minted.token).unwrap(),
            Some(minted.record.principal_id)
        );
    }
}
