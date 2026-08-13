//! # lazybox-entitlement
//!
//! The entitlement gate for the relay broker.
//!
//! The relay is lazybox's payment-enforcement point: it brokers a
//! connection only for an account with an active subscription. Before
//! brokering, the relay resolves the connecting account through an
//! [`EntitlementGate`] and refuses on an [`Entitlement::Inactive`]
//! decision.
//!
//! Hosted relays wire [`PlatformEntitlementGate`] to lazybox-platform.
//! Self-hosted relays retain [`AllowAll`] when no platform configuration
//! is present.

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;
use tokio::time::Instant;

const PLATFORM_TIMEOUT: Duration = Duration::from_secs(2);
const ACTIVE_TTL: Duration = Duration::from_secs(60);
const INACTIVE_TTL: Duration = Duration::from_secs(15);
const CHECK_PATH: &str = "/v1/relay/entitlements/check";

/// The account an incoming relay connection claims to act on behalf of.
///
/// Opaque to this crate. The hosted relay passes the box's base64 Ed25519
/// public key, which is also the platform cache and subscription key.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct AccountId(pub String);

impl AccountId {
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Whether an account may have the relay broker a connection on its
/// behalf.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Entitlement {
    /// Active subscription — the relay may broker.
    Active,
    /// No active subscription — the relay must refuse. `reason` is a
    /// human-readable explanation suitable for the client-side
    /// "Upgrade to connect remotely" UX.
    Inactive { reason: String },
}

impl Entitlement {
    /// `true` only for [`Entitlement::Active`].
    pub fn is_active(&self) -> bool {
        matches!(self, Entitlement::Active)
    }
}

/// A failure to determine entitlement — the lookup itself broke (the
/// subscription service was unreachable, returned garbage, etc.), as
/// opposed to a definitive [`Entitlement::Inactive`] answer.
///
/// The relay must fail closed on this: a payment-enforcement point that
/// brokers when it cannot verify a subscription is not enforcing.
#[derive(Debug, thiserror::Error)]
pub enum EntitlementError {
    #[error("entitlement lookup failed: {0}")]
    Lookup(String),
}

/// Resolves whether an account is entitled to the relay's brokering.
///
/// The relay holds a gate as a trait object (`Box<dyn EntitlementGate>`)
/// so self-hosted and platform-backed checks are interchangeable without
/// touching the broker — hence the boxed-future return, which keeps the
/// trait dyn-compatible.
pub trait EntitlementGate: Send + Sync {
    fn check<'a>(
        &'a self,
        account: &'a AccountId,
    ) -> Pin<Box<dyn Future<Output = Result<Entitlement, EntitlementError>> + Send + 'a>>;
}

/// Stub gate that treats every account as entitled.
///
/// Self-hosted relays use this gate. It never errors and never denies.
#[derive(Debug, Clone, Copy, Default)]
pub struct AllowAll;

impl EntitlementGate for AllowAll {
    fn check<'a>(
        &'a self,
        _account: &'a AccountId,
    ) -> Pin<Box<dyn Future<Output = Result<Entitlement, EntitlementError>> + Send + 'a>> {
        Box::pin(async { Ok(Entitlement::Active) })
    }
}

#[derive(Debug)]
struct CacheEntry {
    entitlement: Entitlement,
    expires_at: Instant,
}

/// An entitlement gate backed by lazybox-platform's relay subscription API.
pub struct PlatformEntitlementGate {
    http: reqwest::Client,
    endpoint: String,
    api_key: String,
    cache: Mutex<HashMap<AccountId, CacheEntry>>,
    refreshes: Mutex<HashMap<AccountId, Arc<Mutex<()>>>>,
}

#[derive(Serialize)]
struct CheckRequest<'a> {
    box_public_key: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    device_public_key: Option<&'a str>,
}

#[derive(Deserialize)]
struct CheckResponse {
    active: bool,
    #[serde(rename = "plan")]
    _plan: String,
    reason: String,
    #[serde(rename = "checked_at")]
    _checked_at: String,
}

impl PlatformEntitlementGate {
    pub fn new(platform_url: impl Into<String>, api_key: impl Into<String>) -> Self {
        let platform_url = platform_url.into();
        Self {
            http: reqwest::Client::new(),
            endpoint: format!("{}{CHECK_PATH}", platform_url.trim_end_matches('/')),
            api_key: api_key.into(),
            cache: Mutex::new(HashMap::new()),
            refreshes: Mutex::new(HashMap::new()),
        }
    }

    async fn check_cached(&self, account: &AccountId) -> Result<Entitlement, EntitlementError> {
        if let Some(entitlement) = self.cached_entitlement(account).await {
            return Ok(entitlement);
        }

        let refresh = {
            let mut refreshes = self.refreshes.lock().await;
            Arc::clone(
                refreshes
                    .entry(account.clone())
                    .or_insert_with(|| Arc::new(Mutex::new(()))),
            )
        };
        let result = {
            let _refresh_guard = refresh.lock().await;
            match self.cached_entitlement(account).await {
                Some(entitlement) => Ok(entitlement),
                None => self.refresh(account).await,
            }
        };
        self.release_refresh(account, &refresh).await;
        result
    }

    async fn cached_entitlement(&self, account: &AccountId) -> Option<Entitlement> {
        let now = Instant::now();
        let mut cache = self.cache.lock().await;
        cache.retain(|_, entry| entry.expires_at > now);
        cache.get(account).map(|entry| entry.entitlement.clone())
    }

    async fn refresh(&self, account: &AccountId) -> Result<Entitlement, EntitlementError> {
        let entitlement = self.fetch(account).await?;
        let ttl = match entitlement {
            Entitlement::Active => ACTIVE_TTL,
            Entitlement::Inactive { .. } => INACTIVE_TTL,
        };
        self.cache.lock().await.insert(
            account.clone(),
            CacheEntry {
                entitlement: entitlement.clone(),
                expires_at: Instant::now() + ttl,
            },
        );
        Ok(entitlement)
    }

    async fn release_refresh(&self, account: &AccountId, refresh: &Arc<Mutex<()>>) {
        let mut refreshes = self.refreshes.lock().await;
        if Arc::strong_count(refresh) == 2
            && refreshes
                .get(account)
                .is_some_and(|current| Arc::ptr_eq(current, refresh))
        {
            refreshes.remove(account);
        }
    }

    async fn fetch(&self, account: &AccountId) -> Result<Entitlement, EntitlementError> {
        let request = async {
            let response = self
                .http
                .post(&self.endpoint)
                .bearer_auth(&self.api_key)
                .json(&CheckRequest {
                    box_public_key: account.as_str(),
                    device_public_key: None,
                })
                .send()
                .await
                .map_err(|error| error.to_string())?;
            if response.status() != reqwest::StatusCode::OK {
                return Err(format!("platform returned HTTP {}", response.status()));
            }
            response
                .json::<CheckResponse>()
                .await
                .map_err(|error| error.to_string())
        };

        let response = tokio::time::timeout(PLATFORM_TIMEOUT, request)
            .await
            .map_err(|_| EntitlementError::Lookup("platform request timed out".into()))?
            .map_err(EntitlementError::Lookup)?;
        if response.active {
            Ok(Entitlement::Active)
        } else {
            Ok(Entitlement::Inactive {
                reason: response.reason,
            })
        }
    }
}

impl EntitlementGate for PlatformEntitlementGate {
    fn check<'a>(
        &'a self,
        account: &'a AccountId,
    ) -> Pin<Box<dyn Future<Output = Result<Entitlement, EntitlementError>> + Send + 'a>> {
        Box::pin(self.check_cached(account))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn allow_all_entitles_every_account() {
        let gate = AllowAll;
        for id in ["", "acct_1", "acct_2"] {
            let decision = gate.check(&AccountId::new(id)).await.unwrap();
            assert_eq!(decision, Entitlement::Active);
            assert!(decision.is_active());
        }
    }

    #[test]
    fn inactive_is_not_active() {
        let decision = Entitlement::Inactive {
            reason: "no active subscription".into(),
        };
        assert!(!decision.is_active());
    }

    #[test]
    fn account_id_round_trips() {
        let id = AccountId::new("acct_42");
        assert_eq!(id.as_str(), "acct_42");
        assert_eq!(id, AccountId("acct_42".into()));
    }

    /// A denying gate proves the trait expresses refusal — the seam the
    /// real licensing check fills. The relay treats both `Inactive` and
    /// a `Lookup` error as "do not broker" (fail closed).
    struct DenyAccount(&'static str);

    impl EntitlementGate for DenyAccount {
        fn check<'a>(
            &'a self,
            account: &'a AccountId,
        ) -> Pin<Box<dyn Future<Output = Result<Entitlement, EntitlementError>> + Send + 'a>>
        {
            Box::pin(async move {
                if account.as_str() == self.0 {
                    Ok(Entitlement::Inactive {
                        reason: "no active subscription".into(),
                    })
                } else {
                    Ok(Entitlement::Active)
                }
            })
        }
    }

    /// A gate whose backing subscription service is unreachable. Proves
    /// the `Lookup` error path is constructible and distinct from a
    /// definitive `Inactive` answer — the fail-closed signal the relay
    /// keys on.
    struct AlwaysUnavailable;

    impl EntitlementGate for AlwaysUnavailable {
        fn check<'a>(
            &'a self,
            _account: &'a AccountId,
        ) -> Pin<Box<dyn Future<Output = Result<Entitlement, EntitlementError>> + Send + 'a>>
        {
            Box::pin(async {
                Err(EntitlementError::Lookup(
                    "subscription service unreachable".into(),
                ))
            })
        }
    }

    #[tokio::test]
    async fn a_gate_can_deny() {
        let gate = DenyAccount("acct_lapsed");
        assert!(
            !gate
                .check(&AccountId::new("acct_lapsed"))
                .await
                .unwrap()
                .is_active()
        );
        assert!(
            gate.check(&AccountId::new("acct_paid"))
                .await
                .unwrap()
                .is_active()
        );
    }

    #[tokio::test]
    async fn a_gate_can_fail_lookup() {
        let gate = AlwaysUnavailable;
        let error = gate.check(&AccountId::new("acct")).await.unwrap_err();
        assert!(matches!(error, EntitlementError::Lookup(_)));
        assert_eq!(
            error.to_string(),
            "entitlement lookup failed: subscription service unreachable"
        );
    }

    /// The seam's whole purpose: the relay holds gates behind
    /// `Box<dyn EntitlementGate>` and swaps stub for real without
    /// touching the broker. This exercises dynamic dispatch, which an
    /// `impl Future` (RPITIT) signature would not permit.
    #[tokio::test]
    async fn gate_is_usable_as_a_trait_object() {
        let gates: Vec<Box<dyn EntitlementGate>> =
            vec![Box::new(AllowAll), Box::new(DenyAccount("acct_lapsed"))];
        assert!(
            gates[0]
                .check(&AccountId::new("anyone"))
                .await
                .unwrap()
                .is_active()
        );
        assert!(
            !gates[1]
                .check(&AccountId::new("acct_lapsed"))
                .await
                .unwrap()
                .is_active()
        );
    }
}
