//! # lazybox-identity
//!
//! Per-device identity for a lazybox box (daemon). Three pieces:
//!
//! - [`BoxIdentity`] — the box's persistent Ed25519 keypair on disk.
//!   Its public key is the box's stable identity (pinned by a pairing
//!   QR so a relay cannot impersonate the box).
//! - [`DeviceRegistry`] — revocable per-device credentials. The box
//!   mints one per paired device, verifies presented tokens, and can
//!   revoke exactly one device (a lost phone) without touching others.
//! - [`DeviceKeystore`] — where a minted secret is held: the macOS/iOS
//!   Keychain on Apple platforms, an owner-only file elsewhere.
//!
//! A device's principal id is `device:<id>` ([`device_principal_id`]),
//! which scopes its provider credentials in the server's credential
//! store — promoting `PrincipalId` beyond the single hardwired `local`.

mod box_identity;
mod device;
mod keystore;

pub use box_identity::{BoxIdentity, BoxIdentityError, verify, verify_base64};
pub use device::{
    DeviceRecord, DeviceRegistry, DeviceRegistryError, MintedDevice, device_principal_id,
};
pub use keystore::{
    DeviceKeystore, FileKeystore, KEYSTORE_SERVICE, KeystoreError, MemoryKeystore, default_keystore,
};

#[cfg(any(target_os = "macos", target_os = "ios"))]
pub use keystore::AppleKeychain;
