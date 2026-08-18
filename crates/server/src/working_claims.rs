//! Ownership memory for GitHub fleet claims.
//!
//! The upstream `working` label is a shared binary signal. It carries no
//! owner, so clearing every label seen by this daemon would erase a claim
//! that another machine acquired. This registry records the only distinction
//! the daemon can prove: whether it successfully applied the label itself or
//! merely observed a pre-existing external claim.
//!
//! This is intentionally conservative across daemon restarts: provenance is
//! not persisted because a binary label cannot prove which machine owns it.
//! A recovered pre-existing label is preserved rather than risk destructive
//! adoption. Owner-qualified, crash-recoverable claims are tracked in #1180.

use lazybox_core::{Task, TaskId, WorkspaceKey};
use std::collections::HashMap;
use std::sync::Arc;

#[derive(Debug, Clone)]
enum Ownership {
    /// This process successfully applied the upstream label.
    Owned(WorkingClaimTarget),
    /// The label was already present when this process joined the task.
    External,
    /// This process cleared its label, but the optimistic local projection
    /// may still be stale if the following store commit failed.
    Cleared,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AcquirePlan {
    Apply,
    KeepOwned,
    PreserveExternal,
}

#[derive(Debug, Clone)]
pub(crate) enum ReleasePlan {
    ClearOwned(WorkingClaimTarget),
    PreserveExternal,
    NothingOwned,
}

/// Process-local claim ownership, keyed by workspace.
///
/// All lifecycle callers already serialize through the per-workspace agent
/// lock. The small synchronous mutex protects only this in-memory map and is
/// never held across provider or store I/O.
#[derive(Debug, Clone, Default)]
pub(crate) struct WorkingClaimRegistry {
    ownership: Arc<parking_lot::Mutex<HashMap<WorkspaceKey, Ownership>>>,
}

/// Minimal immutable identity needed to clear the same upstream task later.
/// The workspace headline may change from an issue to its PR while an agent
/// runs, so storing only the workspace key would target the wrong object.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct WorkingClaimTarget {
    pub(crate) id: TaskId,
    pub(crate) repo: String,
}

impl WorkingClaimTarget {
    pub(crate) fn from_task(task: &Task) -> Option<Self> {
        Some(Self {
            id: task.id.clone(),
            repo: task.repo.clone()?,
        })
    }
}

impl WorkingClaimRegistry {
    /// Decide whether starting a local claiming agent needs an upstream
    /// mutation. A pre-existing label is recorded as external and must never
    /// be cleared by this process.
    pub(crate) fn plan_acquire(
        &self,
        workspace: &WorkspaceKey,
        label_is_present: bool,
    ) -> AcquirePlan {
        let mut ownership = self.ownership.lock();
        match (ownership.get(workspace), label_is_present) {
            (Some(Ownership::Owned(_)), true) => AcquirePlan::KeepOwned,
            (Some(Ownership::External), true) => AcquirePlan::PreserveExternal,
            // A cleared tombstone outranks a stale local label projection.
            // Re-apply before a new agent starts.
            (Some(Ownership::Cleared), _) | (_, false) => AcquirePlan::Apply,
            (None, true) => {
                ownership.insert(workspace.clone(), Ownership::External);
                AcquirePlan::PreserveExternal
            }
        }
    }

    /// Record that the upstream apply completed. Called before the local
    /// projection commit so a store failure cannot make a later teardown
    /// erase ownership knowledge.
    pub(crate) fn record_acquired(&self, workspace: &WorkspaceKey, target: WorkingClaimTarget) {
        self.ownership
            .lock()
            .insert(workspace.clone(), Ownership::Owned(target));
    }

    /// Decide whether the last local agent may clear the upstream label.
    /// External ownership is forgotten after it is preserved; a later spawn
    /// will observe the still-present label afresh.
    pub(crate) fn plan_release(&self, workspace: &WorkspaceKey) -> ReleasePlan {
        let mut ownership = self.ownership.lock();
        match ownership.get(workspace) {
            Some(Ownership::Owned(target)) => ReleasePlan::ClearOwned(target.clone()),
            Some(Ownership::External) => {
                ownership.remove(workspace);
                ReleasePlan::PreserveExternal
            }
            Some(Ownership::Cleared) | None => ReleasePlan::NothingOwned,
        }
    }

    /// Record a successful remote clear. The tombstone defends against a
    /// failed optimistic store commit making the old label look external on
    /// an immediate subsequent spawn.
    pub(crate) fn record_cleared(&self, workspace: &WorkspaceKey) {
        self.ownership
            .lock()
            .insert(workspace.clone(), Ownership::Cleared);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key() -> WorkspaceKey {
        WorkspaceKey::new("github-owner-repo-42")
    }

    fn target(number: u64) -> WorkingClaimTarget {
        WorkingClaimTarget {
            id: TaskId {
                source: lazybox_core::GITHUB_SOURCE.into(),
                key: format!("owner/repo#{number}"),
            },
            repo: "owner/repo".into(),
        }
    }

    #[test]
    fn preexisting_claim_is_preserved_on_release() {
        let registry = WorkingClaimRegistry::default();
        let key = key();

        assert_eq!(
            registry.plan_acquire(&key, true),
            AcquirePlan::PreserveExternal
        );
        assert!(matches!(
            registry.plan_release(&key),
            ReleasePlan::PreserveExternal
        ));
        assert!(matches!(
            registry.plan_release(&key),
            ReleasePlan::NothingOwned
        ));
    }

    #[test]
    fn only_a_successfully_acquired_claim_can_be_cleared() {
        let registry = WorkingClaimRegistry::default();
        let key = key();

        assert_eq!(registry.plan_acquire(&key, false), AcquirePlan::Apply);
        assert!(matches!(
            registry.plan_release(&key),
            ReleasePlan::NothingOwned
        ));

        registry.record_acquired(&key, target(42));
        assert_eq!(registry.plan_acquire(&key, true), AcquirePlan::KeepOwned);
        let ReleasePlan::ClearOwned(target) = registry.plan_release(&key) else {
            panic!("owned claim must be clearable");
        };
        assert_eq!(target.id.key, "owner/repo#42");
    }

    #[test]
    fn cleared_tombstone_forces_reapply_despite_a_stale_projection() {
        let registry = WorkingClaimRegistry::default();
        let key = key();
        registry.record_acquired(&key, target(42));
        registry.record_cleared(&key);

        assert!(matches!(
            registry.plan_release(&key),
            ReleasePlan::NothingOwned
        ));
        assert_eq!(registry.plan_acquire(&key, true), AcquirePlan::Apply);
    }

    #[test]
    fn an_external_claim_that_disappears_can_be_acquired_locally() {
        let registry = WorkingClaimRegistry::default();
        let key = key();
        assert_eq!(
            registry.plan_acquire(&key, true),
            AcquirePlan::PreserveExternal
        );
        assert_eq!(registry.plan_acquire(&key, false), AcquirePlan::Apply);
        registry.record_acquired(&key, target(42));
        assert!(matches!(
            registry.plan_release(&key),
            ReleasePlan::ClearOwned(_)
        ));
    }
}
