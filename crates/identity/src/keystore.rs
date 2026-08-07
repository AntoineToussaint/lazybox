//! Device-side secret storage.
//!
//! A minted per-device credential's secret is held in the OS keystore
//! on the machine that owns the credential — the macOS/iOS Keychain on
//! Apple platforms, an owner-only file elsewhere (headless boxes rarely
//! run a Secret Service daemon). The box keeps only a salted hash of the
//! secret in its [`crate::DeviceRegistry`]; the plaintext lives here so a
//! pairing UI can re-present it and revocation can purge it.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

/// Keychain/service label under which every lazybox device secret is
/// filed. Apple keychains key a secret by `(service, account)`; the
/// account is the device id.
pub const KEYSTORE_SERVICE: &str = "ai.lazybox.device";

#[derive(Debug, thiserror::Error)]
pub enum KeystoreError {
    #[error("keystore has no entry for `{0}`")]
    NotFound(String),
    #[error("keystore backend error: {0}")]
    Backend(String),
}

/// Stores a device's own credential secret, keyed by device id.
pub trait DeviceKeystore: Send + Sync {
    fn set(&self, key: &str, secret: &str) -> Result<(), KeystoreError>;
    fn get(&self, key: &str) -> Result<String, KeystoreError>;
    /// Remove an entry. Deleting a missing key is not an error — a
    /// revoke that races a manual purge must still succeed.
    fn delete(&self, key: &str) -> Result<(), KeystoreError>;
}

/// The platform-appropriate keystore: the Apple Keychain on macOS/iOS,
/// an owner-only file under `dir` everywhere else.
pub fn default_keystore(dir: impl AsRef<Path>) -> Box<dyn DeviceKeystore> {
    #[cfg(any(target_os = "macos", target_os = "ios"))]
    {
        let _ = dir;
        Box::new(AppleKeychain::new(KEYSTORE_SERVICE))
    }
    #[cfg(not(any(target_os = "macos", target_os = "ios")))]
    {
        Box::new(FileKeystore::new(dir))
    }
}

/// In-memory keystore for tests and ephemeral registries.
#[derive(Default)]
pub struct MemoryKeystore {
    inner: Mutex<HashMap<String, String>>,
}

impl MemoryKeystore {
    pub fn new() -> Self {
        Self::default()
    }
}

impl DeviceKeystore for MemoryKeystore {
    fn set(&self, key: &str, secret: &str) -> Result<(), KeystoreError> {
        self.lock().insert(key.to_string(), secret.to_string());
        Ok(())
    }

    fn get(&self, key: &str) -> Result<String, KeystoreError> {
        self.lock()
            .get(key)
            .cloned()
            .ok_or_else(|| KeystoreError::NotFound(key.to_string()))
    }

    fn delete(&self, key: &str) -> Result<(), KeystoreError> {
        self.lock().remove(key);
        Ok(())
    }
}

impl MemoryKeystore {
    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, String>> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// Owner-only file keystore: one `<key>.secret` file per entry, mode
/// `0600`. The fallback where no OS keystore is available.
pub struct FileKeystore {
    dir: PathBuf,
}

impl FileKeystore {
    pub fn new(dir: impl AsRef<Path>) -> Self {
        Self {
            dir: dir.as_ref().join("secrets"),
        }
    }

    fn entry_path(&self, key: &str) -> PathBuf {
        self.dir.join(format!("{}.secret", sanitize(key)))
    }
}

impl DeviceKeystore for FileKeystore {
    fn set(&self, key: &str, secret: &str) -> Result<(), KeystoreError> {
        std::fs::create_dir_all(&self.dir).map_err(|e| KeystoreError::Backend(e.to_string()))?;
        let path = self.entry_path(key);
        write_owner_only(&path, secret.as_bytes())
            .map_err(|e| KeystoreError::Backend(e.to_string()))
    }

    fn get(&self, key: &str) -> Result<String, KeystoreError> {
        match std::fs::read_to_string(self.entry_path(key)) {
            Ok(secret) => Ok(secret),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                Err(KeystoreError::NotFound(key.to_string()))
            }
            Err(e) => Err(KeystoreError::Backend(e.to_string())),
        }
    }

    fn delete(&self, key: &str) -> Result<(), KeystoreError> {
        match std::fs::remove_file(self.entry_path(key)) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(KeystoreError::Backend(e.to_string())),
        }
    }
}

/// A device id is random hex, but harden the file path anyway so a
/// crafted key can never escape the keystore directory.
fn sanitize(key: &str) -> String {
    key.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
                c
            } else {
                '-'
            }
        })
        .collect()
}

fn write_owner_only(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(path)?;
    file.write_all(bytes)
}

#[cfg(any(target_os = "macos", target_os = "ios"))]
pub struct AppleKeychain {
    service: String,
}

#[cfg(any(target_os = "macos", target_os = "ios"))]
impl AppleKeychain {
    pub fn new(service: impl Into<String>) -> Self {
        Self {
            service: service.into(),
        }
    }
}

#[cfg(any(target_os = "macos", target_os = "ios"))]
impl DeviceKeystore for AppleKeychain {
    fn set(&self, key: &str, secret: &str) -> Result<(), KeystoreError> {
        security_framework::passwords::set_generic_password(&self.service, key, secret.as_bytes())
            .map_err(|e| KeystoreError::Backend(e.to_string()))
    }

    fn get(&self, key: &str) -> Result<String, KeystoreError> {
        match security_framework::passwords::get_generic_password(&self.service, key) {
            Ok(bytes) => {
                String::from_utf8(bytes).map_err(|e| KeystoreError::Backend(e.to_string()))
            }
            Err(e) if e.code() == security_framework_sys::base::errSecItemNotFound => {
                Err(KeystoreError::NotFound(key.to_string()))
            }
            Err(e) => Err(KeystoreError::Backend(e.to_string())),
        }
    }

    fn delete(&self, key: &str) -> Result<(), KeystoreError> {
        match security_framework::passwords::delete_generic_password(&self.service, key) {
            Ok(()) => Ok(()),
            Err(e) if e.code() == security_framework_sys::base::errSecItemNotFound => Ok(()),
            Err(e) => Err(KeystoreError::Backend(e.to_string())),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn assert_roundtrip(store: &dyn DeviceKeystore) {
        assert!(matches!(
            store.get("dev-a"),
            Err(KeystoreError::NotFound(_))
        ));
        store.set("dev-a", "secret-a").unwrap();
        store.set("dev-b", "secret-b").unwrap();
        assert_eq!(store.get("dev-a").unwrap(), "secret-a");
        assert_eq!(store.get("dev-b").unwrap(), "secret-b");

        // Overwrite replaces.
        store.set("dev-a", "secret-a2").unwrap();
        assert_eq!(store.get("dev-a").unwrap(), "secret-a2");

        // Deleting one leaves the other; deleting twice is fine.
        store.delete("dev-a").unwrap();
        store.delete("dev-a").unwrap();
        assert!(matches!(
            store.get("dev-a"),
            Err(KeystoreError::NotFound(_))
        ));
        assert_eq!(store.get("dev-b").unwrap(), "secret-b");
    }

    #[test]
    fn memory_keystore_roundtrips() {
        assert_roundtrip(&MemoryKeystore::new());
    }

    #[test]
    fn file_keystore_roundtrips() {
        let dir = tempfile::tempdir().unwrap();
        assert_roundtrip(&FileKeystore::new(dir.path()));
    }

    #[cfg(unix)]
    #[test]
    fn file_keystore_secret_is_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let store = FileKeystore::new(dir.path());
        store.set("dev-a", "secret-a").unwrap();
        let mode = std::fs::metadata(store.entry_path("dev-a"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
    }
}
