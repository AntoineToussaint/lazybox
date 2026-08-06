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
//! Real subscription checks (via codefly / saas-starter) arrive with
//! the licensing work; until then, wire [`AllowAll`] so the relay path
//! is exercisable end to end without gating. Swapping in a real gate is
//! a one-line change at the relay: it holds the trait, not a concrete
//! implementation.

use std::future::Future;

/// The account an incoming relay connection claims to act on behalf of.
///
/// Opaque to this crate — the relay derives it from the connecting
/// device's credential; the gate maps it to a subscription.
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
/// Implement this over the real subscription service when licensing
/// lands. The relay holds a gate as a trait object so the stub and the
/// real check are interchangeable without touching the broker.
pub trait EntitlementGate: Send + Sync {
    fn check(
        &self,
        account: &AccountId,
    ) -> impl Future<Output = Result<Entitlement, EntitlementError>> + Send;
}

/// Stub gate that treats every account as entitled.
///
/// The wired default until real licensing lands, per the "stub /
/// allow-all first" plan. It never errors and never denies.
#[derive(Debug, Clone, Copy, Default)]
pub struct AllowAll;

impl EntitlementGate for AllowAll {
    async fn check(&self, _account: &AccountId) -> Result<Entitlement, EntitlementError> {
        Ok(Entitlement::Active)
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
        async fn check(&self, account: &AccountId) -> Result<Entitlement, EntitlementError> {
            if account.as_str() == self.0 {
                Ok(Entitlement::Inactive {
                    reason: "no active subscription".into(),
                })
            } else {
                Ok(Entitlement::Active)
            }
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
}
