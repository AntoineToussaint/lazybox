//! QR / link encoding and a one-time, short-TTL pairing code (#894).
//!
//! To pair a new device the box shows a link (encoded as a QR code by the
//! caller) carrying three things: the relay URL to dial, the box's public
//! identity key, and a short-lived one-time code. A client opens the link,
//! reaches the box through the relay, and runs an authenticated key exchange
//! pinned to the box public key — so a malicious relay cannot MITM — proving
//! knowledge of the code. The box then mints a per-device credential.
//!
//! This module owns the two transport-agnostic pieces: the [`PairingLink`]
//! wire encoding and the [`PairingCode`] lifetime. The key exchange itself
//! and the per-device credential store arrive with the E2E channel (#891) and
//! per-device identity (#892); until then `box_pubkey` is opaque bytes.
//!
//! The link is `lazybox://pair#<base64url(bincode(payload))>` — a single
//! opaque token, so a QR encoder just renders the string and a screenshot of
//! it leaks only a code that expires. `#` (fragment) keeps the secret out of
//! request paths were the link ever handled by a URL router.

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};

/// URL scheme + fragment prefix every pairing link carries.
pub const PAIRING_URL_PREFIX: &str = "lazybox://pair#";

/// Default lifetime of a pairing code, in seconds: long enough to scan and
/// connect, short enough that a leaked screenshot is not a standing risk.
pub const DEFAULT_CODE_TTL_SECS: i64 = 120;

/// Crockford base32 — uppercase, no `I`/`L`/`O`/`U`, so a code read off a
/// screen and typed on another device is unambiguous.
const CROCKFORD: &[u8; 32] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";

/// Upper bound on a decoded pairing link. A real link is a short relay URL, a
/// 32-byte key, and a tiny code — well under this. Bounding the bincode decode
/// stops a crafted link (e.g. one whose `box_pubkey` length prefix claims
/// gigabytes) from driving a huge allocation, matching the `with_limit`
/// discipline the socket framing uses on untrusted input.
const MAX_PAIRING_LINK_BYTES: usize = 4096;

#[derive(Debug, thiserror::Error)]
pub enum PairingError {
    #[error("not a lazybox pairing link")]
    BadScheme,
    #[error("pairing link payload is not valid base64url: {0}")]
    Base64(#[from] base64::DecodeError),
    #[error("pairing link could not be encoded")]
    Encode,
    #[error("pairing link payload could not be decoded")]
    Decode,
    #[error("could not read system randomness")]
    Random,
}

/// A one-time pairing code with an expiry. "One-time" is enforced by the box
/// consuming it after a successful pair; this type owns only validity —
/// whether a submitted code matches and is still live.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PairingCode {
    pub code: String,
    pub expires_at: DateTime<Utc>,
}

impl PairingCode {
    /// Build a code from an explicit 5-byte seed (40 bits → 8 base32 chars,
    /// no padding). Pure and deterministic, so tests don't touch the RNG.
    pub fn from_seed(seed: [u8; 5], expires_at: DateTime<Utc>) -> Self {
        Self {
            code: encode_crockford(seed),
            expires_at,
        }
    }

    /// Mint a fresh random code valid for `ttl` from `now`.
    pub fn generate(now: DateTime<Utc>, ttl: Duration) -> Result<Self, PairingError> {
        let mut seed = [0u8; 5];
        getrandom::fill(&mut seed).map_err(|_| PairingError::Random)?;
        Ok(Self::from_seed(seed, now + ttl))
    }

    pub fn is_expired(&self, now: DateTime<Utc>) -> bool {
        now >= self.expires_at
    }

    /// A submitted code is accepted only if it matches exactly and has not
    /// expired.
    pub fn accepts(&self, submitted: &str, now: DateTime<Utc>) -> bool {
        !self.is_expired(now) && self.code == submitted
    }
}

/// Encode 5 bytes as 8 Crockford-base32 characters (5 bits each, MSB first).
fn encode_crockford(bytes: [u8; 5]) -> String {
    let n = u64::from(bytes[0]) << 32
        | u64::from(bytes[1]) << 24
        | u64::from(bytes[2]) << 16
        | u64::from(bytes[3]) << 8
        | u64::from(bytes[4]);
    let mut out = String::with_capacity(8);
    for i in 0..8 {
        let shift = 5 * (7 - i);
        out.push(CROCKFORD[((n >> shift) & 0x1f) as usize] as char);
    }
    out
}

/// Everything a client needs to reach and authenticate the box.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PairingLink {
    /// Relay endpoint the client dials to reach the box (#893).
    pub relay_url: String,
    /// Box public identity key. Opaque bytes here; the concrete key type
    /// lands with per-device identity (#892). The client pins the box to
    /// this key across the relay so a compromised relay cannot MITM.
    pub box_pubkey: Vec<u8>,
    /// The one-time short-TTL code the client proves knowledge of. Just the
    /// code string — the box holds the [`PairingCode`] (with its expiry) and
    /// is the sole authority on validity, so the link carries no expiry the
    /// client might be tempted to trust.
    pub code: String,
}

impl PairingLink {
    /// Encode as `lazybox://pair#<base64url(bincode(self))>`.
    pub fn to_url(&self) -> Result<String, PairingError> {
        let config = bincode::config::standard().with_limit::<MAX_PAIRING_LINK_BYTES>();
        let bytes =
            bincode::serde::encode_to_vec(self, config).map_err(|_| PairingError::Encode)?;
        Ok(format!(
            "{PAIRING_URL_PREFIX}{}",
            URL_SAFE_NO_PAD.encode(bytes)
        ))
    }

    /// Parse a link produced by [`PairingLink::to_url`]. The decode is bounded
    /// (see `MAX_PAIRING_LINK_BYTES`) so a crafted payload cannot force a
    /// large allocation.
    pub fn from_url(url: &str) -> Result<Self, PairingError> {
        let payload = url
            .strip_prefix(PAIRING_URL_PREFIX)
            .ok_or(PairingError::BadScheme)?;
        let bytes = URL_SAFE_NO_PAD.decode(payload)?;
        let config = bincode::config::standard().with_limit::<MAX_PAIRING_LINK_BYTES>();
        let (link, _) =
            bincode::serde::decode_from_slice(&bytes, config).map_err(|_| PairingError::Decode)?;
        Ok(link)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_link() -> PairingLink {
        PairingLink {
            relay_url: "wss://relay.lazybox.ai/box/abc123".to_string(),
            box_pubkey: vec![0xDE, 0xAD, 0xBE, 0xEF, 0x00, 0x11],
            code: PairingCode::from_seed([1, 2, 3, 4, 5], Utc::now()).code,
        }
    }

    #[test]
    fn code_seed_is_eight_crockford_chars() {
        let code = PairingCode::from_seed([0xFF; 5], Utc::now());
        assert_eq!(code.code.len(), 8);
        assert!(code.code.bytes().all(|b| CROCKFORD.contains(&b)));
        // All-ones 40-bit value → all-Z in Crockford base32.
        assert_eq!(code.code, "ZZZZZZZZ");
        // All-zero seed → all-0.
        assert_eq!(PairingCode::from_seed([0; 5], Utc::now()).code, "00000000");
    }

    #[test]
    fn code_accepts_only_exact_and_unexpired() {
        let issued: DateTime<Utc> = "2026-08-06T12:00:00Z".parse().unwrap();
        let code = PairingCode::from_seed([9, 9, 9, 9, 9], issued + Duration::seconds(120));

        let before = issued + Duration::seconds(60);
        let after = issued + Duration::seconds(180);
        assert!(code.accepts(&code.code, before));
        assert!(
            !code.accepts("WRONGONE", before),
            "a mismatched code is rejected"
        );
        assert!(
            !code.accepts(&code.code, after),
            "an expired code is rejected"
        );
    }

    #[test]
    fn is_expired_at_the_boundary() {
        let expires: DateTime<Utc> = "2026-08-06T12:00:00Z".parse().unwrap();
        let code = PairingCode::from_seed([0; 5], expires);
        assert!(!code.is_expired(expires - Duration::seconds(1)));
        assert!(code.is_expired(expires), "expiry is inclusive");
    }

    #[test]
    fn generate_respects_ttl_and_is_random() {
        let now: DateTime<Utc> = "2026-08-06T12:00:00Z".parse().unwrap();
        let a = PairingCode::generate(now, Duration::seconds(90)).unwrap();
        let b = PairingCode::generate(now, Duration::seconds(90)).unwrap();
        assert_eq!(a.expires_at, now + Duration::seconds(90));
        assert_eq!(a.code.len(), 8);
        assert_ne!(
            a.code, b.code,
            "two mints must differ (with overwhelming probability)"
        );
    }

    #[test]
    fn link_round_trips_through_the_url() {
        let link = sample_link();
        let url = link.to_url().unwrap();
        assert!(url.starts_with(PAIRING_URL_PREFIX));
        assert_eq!(PairingLink::from_url(&url).unwrap(), link);
    }

    #[test]
    fn from_url_rejects_a_foreign_scheme() {
        assert!(matches!(
            PairingLink::from_url("https://example.com"),
            Err(PairingError::BadScheme)
        ));
    }

    #[test]
    fn from_url_rejects_corrupt_payload() {
        let mut url = sample_link().to_url().unwrap();
        url.push_str("!!!not-base64!!!");
        assert!(PairingLink::from_url(&url).is_err());
    }

    #[test]
    fn from_url_rejects_an_oversized_payload() {
        // Craft a link whose decoded payload exceeds the limit (a huge
        // box_pubkey), encoding it with an *unbounded* config to bypass
        // to_url's own limit. from_url's bounded decode must refuse it rather
        // than allocate gigabytes.
        let oversized = PairingLink {
            relay_url: "wss://r".to_string(),
            box_pubkey: vec![0u8; MAX_PAIRING_LINK_BYTES * 2],
            code: "AAAAAAAA".to_string(),
        };
        let bytes = bincode::serde::encode_to_vec(&oversized, bincode::config::standard()).unwrap();
        let url = format!("{PAIRING_URL_PREFIX}{}", URL_SAFE_NO_PAD.encode(bytes));
        assert!(matches!(
            PairingLink::from_url(&url),
            Err(PairingError::Decode)
        ));
    }
}
