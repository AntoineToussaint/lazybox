//! Per-workspace automation policies — the single, explicit model that
//! unifies lazybox's "behave automatically on this PR/issue" controls
//! behind one surface (issue #363).
//!
//! ## The three mechanisms (and their precedence)
//!
//! lazybox has three ways a PR/issue can be automated, each with its own
//! provenance. This module owns the per-session overrides and documents
//! how the three compose so nothing acts silently:
//!
//! 1. **merge-on-green** — lazybox's *client-side* arm
//!    ([`crate::Workspace::auto_merge_on_green`]). Fires a merge the
//!    moment your own PR is merge-ready, and only while lazybox runs. It
//!    stays on its own `Workspace` field (already wired end-to-end) but
//!    is presented in the same policies surface.
//! 2. **auto-fix** — spawns an agent to fix failing CI / a merge
//!    conflict. Globally opt-in ([`crate::AutoFixSettings::enabled`]) and
//!    historically opt-*out* per PR via GitHub labels. This module adds
//!    the per-session [`PolicyArm`] override the audit found missing.
//! 3. **GitHub-native auto-merge** — GitHub's own server-side "merge when
//!    ready" ([`crate::Task::auto_merge_enabled`]). lazybox does not set
//!    it; the surface shows it read-only.
//!
//! ### Precedence
//!
//! - **native auto-merge > client merge-on-green.** When GitHub's native
//!   auto-merge is already enabled on a PR, lazybox's client-side
//!   merge-on-green stands down (see `lazybox_tui_core::intent`'s
//!   `should_auto_merge`) — GitHub will land it, so a second client-side
//!   merge is redundant and racy.
//! - **auto-fix per-session [`PolicyArm`]** resolves as
//!   [`auto_fix_permitted`] documents: an explicit `Disarm` beats
//!   everything, an explicit `Arm` overrides a label opt-out, and
//!   `Default` follows the label. The global feature switch is applied
//!   upstream (a globally-disabled feature never runs, whatever the arm).
//! - **auto-fix vs merge-on-green are orthogonal** and apply to different
//!   PR states (red CI / conflict vs. green + ready), so they never
//!   contend for the same PR at the same moment.
//!
//! ### PR vs issue
//!
//! Every policy here targets a PR. merge-on-green and auto-fix are
//! meaningless on an issue-only workspace (nothing to merge, no CI to
//! fix); the UI surfaces them as unavailable there rather than arming a
//! flag that could never fire.

use crate::AutoFixKind;
use serde::{Deserialize, Serialize};

/// Per-session override for an auto-fix behavior. The unified,
/// discoverable replacement for "label opt-out only" — a workspace can
/// now positively arm or explicitly disarm auto-fix for itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum PolicyArm {
    /// Follow the global auto-fix configuration (feature switch + label
    /// opt-out). The safe default: a workspace with no explicit choice
    /// behaves exactly as it did before per-session policies existed.
    #[default]
    Default,
    /// Force this behavior ON for this workspace, overriding a label
    /// opt-out. Still requires the global feature to be enabled — arming
    /// a globally-disabled feature is out of scope (the source never
    /// queues a candidate when the feature is off).
    Arm,
    /// Force this behavior OFF for this workspace. Wins over everything —
    /// an explicit "don't touch this PR" always holds.
    Disarm,
}

impl PolicyArm {
    /// Stable wire/log discriminant.
    pub fn as_str(self) -> &'static str {
        match self {
            PolicyArm::Default => "default",
            PolicyArm::Arm => "arm",
            PolicyArm::Disarm => "disarm",
        }
    }
}

/// The unified per-workspace automation-policy set. Persisted on
/// [`crate::Workspace`] (serde-defaulted, so pre-#363 records read back
/// as all-`Default` and behave unchanged). merge-on-green lives on its
/// own `Workspace` field for back-compat; this struct owns the
/// per-session auto-fix arms.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct AutomationPolicies {
    #[serde(default)]
    pub auto_fix_ci: PolicyArm,
    #[serde(default)]
    pub auto_fix_conflict: PolicyArm,
}

impl AutomationPolicies {
    /// The arm governing `kind`.
    pub fn arm(&self, kind: AutoFixKind) -> PolicyArm {
        match kind {
            AutoFixKind::CiFailure => self.auto_fix_ci,
            AutoFixKind::MergeConflict => self.auto_fix_conflict,
        }
    }

    /// Set the arm governing `kind`.
    pub fn set(&mut self, kind: AutoFixKind, arm: PolicyArm) {
        match kind {
            AutoFixKind::CiFailure => self.auto_fix_ci = arm,
            AutoFixKind::MergeConflict => self.auto_fix_conflict = arm,
        }
    }

    /// Any auto-fix behavior explicitly armed (not `Default`). Drives the
    /// "armed policy" pill so an explicit per-session choice is visible.
    pub fn any_auto_fix_armed(&self) -> bool {
        self.auto_fix_ci == PolicyArm::Arm || self.auto_fix_conflict == PolicyArm::Arm
    }
}

/// Resolve whether a per-session `arm` permits auto-fix, given whether a
/// label currently opts the PR out. The global feature switch is applied
/// separately (upstream, when candidates are queued), so this is purely
/// the per-session × label layer. Precedence, strongest first:
///
/// 1. `Disarm` → never (an explicit "off" beats all).
/// 2. `Arm`    → always (overrides a label opt-out).
/// 3. `Default`→ follow the label opt-out.
pub fn auto_fix_permitted(arm: PolicyArm, label_opted_out: bool) -> bool {
    match arm {
        PolicyArm::Disarm => false,
        PolicyArm::Arm => true,
        PolicyArm::Default => !label_opted_out,
    }
}

/// The `PolicyArm` a menu toggle should land on to flip the *effective*
/// on/off state, given whether a label currently opts the PR out. One
/// keypress moves between armed and disarmed regardless of which
/// underlying variant expresses it:
///
/// - effective-on  → `Disarm` (turn off).
/// - effective-off → `Arm` if a label is the reason (override it), else
///   `Default` (a plain "back to normal, on").
pub fn toggled_arm(arm: PolicyArm, label_opted_out: bool) -> PolicyArm {
    if auto_fix_permitted(arm, label_opted_out) {
        PolicyArm::Disarm
    } else if label_opted_out {
        PolicyArm::Arm
    } else {
        PolicyArm::Default
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_arm_follows_label() {
        assert!(auto_fix_permitted(PolicyArm::Default, false));
        assert!(!auto_fix_permitted(PolicyArm::Default, true));
    }

    #[test]
    fn disarm_always_off_even_without_label() {
        assert!(!auto_fix_permitted(PolicyArm::Disarm, false));
        assert!(!auto_fix_permitted(PolicyArm::Disarm, true));
    }

    #[test]
    fn arm_overrides_label_opt_out() {
        assert!(auto_fix_permitted(PolicyArm::Arm, true));
        assert!(auto_fix_permitted(PolicyArm::Arm, false));
    }

    /// Toggling walks a clean two-state cycle from every starting point.
    #[test]
    fn toggle_flips_effective_state() {
        // Default, no label → on. Toggle → Disarm (off).
        assert_eq!(toggled_arm(PolicyArm::Default, false), PolicyArm::Disarm);
        // Disarm → off. Toggle → Default (on), no label to override.
        assert_eq!(toggled_arm(PolicyArm::Disarm, false), PolicyArm::Default);
        // Default + label → off. Toggle → Arm (override the label).
        assert_eq!(toggled_arm(PolicyArm::Default, true), PolicyArm::Arm);
        // Arm → on. Toggle → Disarm (off).
        assert_eq!(toggled_arm(PolicyArm::Arm, true), PolicyArm::Disarm);
        // Disarm + label → off. Toggle → Arm (turning on must override
        // the label, else it would read as on but never fire).
        assert_eq!(toggled_arm(PolicyArm::Disarm, true), PolicyArm::Arm);
    }

    #[test]
    fn accessors_route_by_kind() {
        let mut p = AutomationPolicies::default();
        assert_eq!(p.arm(AutoFixKind::CiFailure), PolicyArm::Default);
        p.set(AutoFixKind::CiFailure, PolicyArm::Disarm);
        assert_eq!(p.arm(AutoFixKind::CiFailure), PolicyArm::Disarm);
        assert_eq!(p.arm(AutoFixKind::MergeConflict), PolicyArm::Default);
        p.set(AutoFixKind::MergeConflict, PolicyArm::Arm);
        assert!(p.any_auto_fix_armed());
    }

    /// Pre-#363 workspace JSON has no `policies` key; it must read back
    /// as all-`Default` (behavior-preserving).
    #[test]
    fn deserializes_missing_as_default() {
        let p: AutomationPolicies = serde_json::from_str("{}").unwrap();
        assert_eq!(p, AutomationPolicies::default());
        assert_eq!(p.auto_fix_ci, PolicyArm::Default);
    }
}
