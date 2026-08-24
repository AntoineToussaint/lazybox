//! Auto-inject fix work on CI failure or merge conflict.
//!
//! When a PR lazybox tracks fails CI or develops a merge conflict, the
//! next step — spawn an agent, point it at the failure, push a fix —
//! is mechanical. This module holds the *pure* decision layer: given a
//! [`Task`] and the user's [`AutoFixSettings`], should lazybox kick off
//! an auto-fix, and of which kind? The stateful pieces (attempt
//! counting, cooldown, posting the PR comment, spawning the agent)
//! live in `lazybox-server`'s polling layer, which calls
//! [`evaluate_auto_fix`] and then layers the guards that need the
//! store on top.
//!
//! ## Why the guards live here (and not in the server)
//!
//! The "should we touch this PR at all" predicate is the part most
//! worth unit-testing exhaustively — it's the difference between
//! lazybox quietly fixing your own broken build and lazybox rewriting a
//! teammate's PR uninvited. Keeping it pure (no store, no network)
//! means every guard gets a fast deterministic test, and the server
//! layer only has to test the stateful parts (cooldown / max-attempts)
//! once.

use crate::policy::{PolicyArm, auto_fix_permitted};
use crate::{CiStatus, ReviewStatus, Task, TaskRole, TaskState};
use serde::{Deserialize, Serialize};
use std::time::Duration;

/// Which failure mode triggered an auto-fix. Determines the prompt
/// the agent gets and the key the attempt counter is tracked under
/// (CI failures and merge conflicts get *separate* attempt budgets —
/// a flaky test shouldn't burn the conflict budget and vice versa).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "desktop-contract", derive(ts_rs::TS))]
pub enum AutoFixKind {
    /// A required check transitioned to failing.
    CiFailure,
    /// The PR no longer applies cleanly onto its base.
    MergeConflict,
}

impl AutoFixKind {
    /// Stable discriminant used to namespace the per-PR attempt
    /// counter in the store (`autofix:<session-key>:<kind>`). Must
    /// stay stable across releases or a redeploy resets everyone's
    /// counters.
    pub fn store_key(self) -> &'static str {
        match self {
            Self::CiFailure => "ci",
            Self::MergeConflict => "conflict",
        }
    }

    /// Human phrase for the PR comment / log line.
    pub fn describe(self) -> &'static str {
        match self {
            Self::CiFailure => "fixing CI",
            Self::MergeConflict => "resolving merge conflicts",
        }
    }
}

/// Runtime settings for the auto-fix feature, resolved from
/// `~/.lazybox/config.yaml` (`auto_fix:` block). Lives in `lazybox-core`
/// (not `lazybox-config`) so the pure guard and the server layer share
/// one type without `lazybox-config` leaking into either; `lazybox-config`
/// depends on core and converts its YAML form into this.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AutoFixSettings {
    /// Master switch. Defaults to `false` — auto-fix pushes commits to
    /// your PRs with no human nudge, so it's strictly opt-in.
    pub enabled: bool,
    /// Labels that opt a PR out entirely. Case-insensitive. A PR
    /// carrying any of these is never auto-fixed.
    pub opt_out_labels: Vec<String>,
    /// Max auto-fix attempts per PR per `window`, *per kind*. After
    /// this many the PR is surfaced for manual attention instead of
    /// looping forever on a flaky test or unresolvable conflict.
    pub max_attempts: u32,
    /// Minimum gap between two attempts on the same PR+kind. Must be
    /// larger than the poll interval or every sweep re-fires; the
    /// default (1h) comfortably clears the ~10min full-sweep cadence.
    pub cooldown: Duration,
    /// Rolling window the `max_attempts` budget is measured over. The
    /// counter resets once this elapses since the first attempt in the
    /// window.
    pub window: Duration,
}

impl Default for AutoFixSettings {
    fn default() -> Self {
        Self {
            enabled: false,
            opt_out_labels: vec!["no-auto-fix".into(), "do-not-lazybox".into()],
            max_attempts: 3,
            cooldown: Duration::from_secs(60 * 60),
            window: Duration::from_secs(24 * 60 * 60),
        }
    }
}

/// Decide whether `task` warrants an auto-fix right now, and of which
/// kind, for a workspace with **no explicit per-session policy** (the
/// [`PolicyArm::Default`] case). Pure — no store, no clock. Returns
/// `None` when any guard blocks; the server layer adds the stateful
/// guards (cooldown, max-attempts) on top of a `Some` result.
///
/// This is the source-path shorthand for
/// [`resolve_auto_fix`]`(task, settings, PolicyArm::Default)` — it
/// exists so callers that have no per-session arm to apply keep a
/// two-argument entry point, while the actual enable/label/candidate
/// composition lives in exactly one place ([`resolve_auto_fix`]).
///
/// Guards, in order (all must pass):
///
/// 1. Feature enabled.
/// 2. Task is a PR (issues have nothing to "fix CI" on).
/// 3. PR is in an actionable state — not merged/closed (a stale CI
///    fail there isn't actionable) and not a **draft** (a draft is
///    explicitly WIP; its CI is expected to be red and the author is
///    mid-work, so auto-fixing would fight them).
/// 4. **Author scope** — `role == Author`: only PRs the configured
///    viewer authored. We never touch a third party's PR. (`role`
///    encodes *why* the PR is in your inbox; `Author` means you opened
///    it. There's no separate author-login field on `Task`, and we
///    don't want one — `role` is the canonical "is this mine" signal.)
/// 5. **Opt-out label** — none of `opt_out_labels` present.
/// 6. **Human in the loop** — no requested reviewers and no
///    changes-requested review. If a person is actively reviewing,
///    hold off so lazybox isn't force-pushing under them.
/// 7. **Merge queue** — not currently in GitHub's merge queue; let the
///    queue resolve.
///
/// Trigger priority: a conflict outranks a CI failure (you can't get
/// clean CI on a branch that won't even merge). When both are present
/// we resolve the conflict first; the next sweep picks up any
/// remaining CI failure.
pub fn evaluate_auto_fix(task: &Task, settings: &AutoFixSettings) -> Option<AutoFixKind> {
    resolve_auto_fix(task, settings, PolicyArm::Default)
}

/// **The** single auto-fix decision. Composes every layer in one place —
/// the global enable switch, the task-shape candidacy
/// ([`auto_fix_candidate`]), the label opt-out
/// ([`is_auto_fix_opted_out`]), and the per-session [`PolicyArm`] — and
/// returns the failure kind to fix, or `None` when any layer blocks.
///
/// `evaluate_auto_fix` is the `PolicyArm::Default` shorthand of this
/// function, and the server's dispatcher gates its (already queued)
/// candidates through [`auto_fix_enabled_and_permitted`], the same
/// enable/label/arm composition this function uses. Keeping the
/// composition here means a future change to how those layers combine
/// (e.g. whether an `Arm` can override the global switch) lands in one
/// spot the tests cover, instead of drifting between core and the daemon.
///
/// Precedence of the per-session arm (see [`auto_fix_permitted`]): an
/// explicit `Disarm` blocks even a clean candidate; an explicit `Arm`
/// overrides a label opt-out; `Default` follows the label. The global
/// enable switch is applied on top of all three — arming a
/// globally-disabled feature never fires.
pub fn resolve_auto_fix(
    task: &Task,
    settings: &AutoFixSettings,
    arm: PolicyArm,
) -> Option<AutoFixKind> {
    let kind = auto_fix_candidate(task)?;
    auto_fix_enabled_and_permitted(settings.enabled, is_auto_fix_opted_out(task, settings), arm)
        .then_some(kind)
}

/// The **policy-layer gate** for an already-shape-eligible candidate:
/// whether it may be auto-fixed given the global enable switch
/// (`enabled`), whether a label currently opts the PR out, and the
/// resolved per-session [`PolicyArm`]. This is the one place the
/// enable + label + arm layers are composed — the daemon dispatcher, the
/// pure `resolve_auto_fix` path, and the TUI policies menu all gate
/// through it, so the "should we touch this PR?" answer can never drift
/// between where it's decided and where it's shown.
///
/// Takes the bare `enabled` flag rather than the whole [`AutoFixSettings`]
/// so a caller that only knows the global switch (the TUI client, which
/// never sees the daemon's full settings) composes the identical rule.
///
/// Split out from [`resolve_auto_fix`] because the daemon resolves the
/// three inputs in two phases separated by the action queue: the source
/// poller computes the candidate + label opt-out from the [`Task`] it has
/// (there is no store there), and the dispatcher resolves the workspace's
/// arm from the store (there is no `Task` there). Both phases feed this
/// single function so the daemon composes enable/label/arm through the
/// exact same code the pure `resolve_auto_fix` path uses, rather than
/// re-deriving the rule (issue #363).
pub fn auto_fix_enabled_and_permitted(
    enabled: bool,
    label_opted_out: bool,
    arm: PolicyArm,
) -> bool {
    enabled && auto_fix_permitted(arm, label_opted_out)
}

/// The **task-shape** half of the auto-fix decision: everything that
/// depends only on the PR's own state, with no reference to the global
/// feature switch, the label opt-out, or per-session
/// [`crate::AutomationPolicies`]. Returns the failure kind a PR is a
/// candidate to auto-fix for, or `None` when its shape rules it out.
///
/// Split out from [`evaluate_auto_fix`] so the per-session policy layer
/// (issue #363) can resolve the label opt-out and the workspace's arm
/// state in the daemon's dispatcher — which has the store the pure
/// source path lacks — while the source still filters down to the small
/// candidate set (your own broken open PRs) here.
///
/// Guards (all must pass):
///
/// 1. Task is a PR (issues have nothing to "fix CI" on).
/// 2. PR is in an actionable state — not merged/closed and not a draft
///    (WIP: its CI is expected to be red and the author is mid-work).
/// 3. **Author scope** — `role == Author`: only PRs the configured
///    viewer authored. We never touch a third party's PR.
/// 4. **Human in the loop** — no requested reviewers and no
///    changes-requested review.
/// 5. **Merge queue** — not currently in GitHub's merge queue.
///
/// Trigger priority: a conflict outranks a CI failure (you can't get
/// clean CI on a branch that won't even merge).
pub fn auto_fix_candidate(task: &Task) -> Option<AutoFixKind> {
    if !task.is_pr() {
        return None;
    }
    // Inactive PRs (nothing to fix) and drafts (WIP — author owns the
    // red CI) are both out of scope.
    if matches!(
        task.state,
        TaskState::Closed | TaskState::Merged | TaskState::Draft
    ) {
        return None;
    }
    // Author scope — only our own PRs.
    if task.role != TaskRole::Author {
        return None;
    }
    // Hold off while a human is actively reviewing: a requested
    // reviewer is waiting, or someone already asked for changes.
    if !task.reviewers.is_empty() || task.review == ReviewStatus::ChangesRequested {
        return None;
    }
    // Don't fight the merge queue.
    if task.is_in_merge_queue {
        return None;
    }
    // Conflict first, then CI failure.
    if task.mergeable.is_conflicting() {
        return Some(AutoFixKind::MergeConflict);
    }
    if task.ci == CiStatus::Failure {
        return Some(AutoFixKind::CiFailure);
    }
    None
}

/// Whether any of `settings.opt_out_labels` (case-insensitive) is
/// present on `task`. This is the **label** half of the auto-fix
/// decision — kept separate so the per-session policy layer can let an
/// explicit [`crate::PolicyArm::Arm`] override it (issue #363) while
/// [`evaluate_auto_fix`] still honors it for the default case.
pub fn is_auto_fix_opted_out(task: &Task, settings: &AutoFixSettings) -> bool {
    task.labels.iter().any(|label| {
        settings
            .opt_out_labels
            .iter()
            .any(|opt| opt.eq_ignore_ascii_case(&label.name))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{CheckRun, Mergeable, ReviewStatus, TaskId};
    use chrono::Utc;

    fn pr() -> Task {
        Task {
            author: String::new(),
            id: TaskId {
                source: "github".into(),
                key: "acme/widget#7".into(),
            },
            title: "Add thing".into(),
            body: None,
            state: TaskState::Open,
            role: TaskRole::Author,
            ci: CiStatus::Failure,
            review: ReviewStatus::None,
            checks: vec![],
            unread_count: 0,
            url: "https://github.com/acme/widget/pull/7".into(),
            repo: Some("acme/widget".into()),
            branch: Some("feature".into()),
            base_branch: Some("main".into()),
            updated_at: Utc::now(),
            created_at: None,
            closed_at: None,
            labels: vec![],
            reviewers: vec![],
            reviews: vec![],
            assignees: vec![],
            auto_merge_enabled: false,
            is_in_merge_queue: false,
            mergeable: Mergeable::new(crate::MergeableState::Mergeable, Utc::now()),
            is_behind_base: false,
            merge_blocked: false,
            approval_policy: Default::default(),
            node_id: None,
            needs_reply: false,
            last_commenter: None,
            recent_activity: vec![],
            additions: 0,
            deletions: 0,
            changed_files: 0,
            kind: None,
            closes_issues: vec![],
            linked_tasks: vec![],
            parent: None,
            priority: None,
            state_label: None,
        }
    }

    fn enabled() -> AutoFixSettings {
        AutoFixSettings {
            enabled: true,
            ..Default::default()
        }
    }

    #[test]
    fn ci_failure_on_own_open_pr_triggers() {
        assert_eq!(
            evaluate_auto_fix(&pr(), &enabled()),
            Some(AutoFixKind::CiFailure)
        );
    }

    #[test]
    fn conflict_triggers_and_outranks_ci() {
        let mut t = pr();
        t.mergeable = Mergeable::new(crate::MergeableState::Conflicting, Utc::now());
        t.ci = CiStatus::Failure;
        assert_eq!(
            evaluate_auto_fix(&t, &enabled()),
            Some(AutoFixKind::MergeConflict)
        );
    }

    #[test]
    fn disabled_never_triggers() {
        let t = pr();
        assert_eq!(evaluate_auto_fix(&t, &AutoFixSettings::default()), None);
    }

    #[test]
    fn green_ci_no_conflict_does_nothing() {
        let mut t = pr();
        t.ci = CiStatus::Success;
        t.mergeable = Mergeable::new(crate::MergeableState::Mergeable, Utc::now());
        assert_eq!(evaluate_auto_fix(&t, &enabled()), None);
    }

    #[test]
    fn pending_or_running_ci_does_not_trigger() {
        let mut t = pr();
        t.ci = CiStatus::Running;
        assert_eq!(evaluate_auto_fix(&t, &enabled()), None);
        t.ci = CiStatus::Pending;
        assert_eq!(evaluate_auto_fix(&t, &enabled()), None);
    }

    #[test]
    fn issues_are_skipped() {
        let mut t = pr();
        t.url = "https://github.com/acme/widget/issues/7".into();
        assert_eq!(evaluate_auto_fix(&t, &enabled()), None);
    }

    #[test]
    fn closed_merged_or_draft_skipped() {
        let mut t = pr();
        t.state = TaskState::Closed;
        assert_eq!(evaluate_auto_fix(&t, &enabled()), None);
        t.state = TaskState::Merged;
        assert_eq!(evaluate_auto_fix(&t, &enabled()), None);
        // Drafts are WIP — the author owns the red CI.
        t.state = TaskState::Draft;
        assert_eq!(evaluate_auto_fix(&t, &enabled()), None);
        // ...even with a conflict.
        t.mergeable = crate::Mergeable::new(crate::MergeableState::Conflicting, Utc::now());
        assert_eq!(evaluate_auto_fix(&t, &enabled()), None);
    }

    #[test]
    fn third_party_pr_skipped() {
        let mut t = pr();
        t.role = TaskRole::Reviewer;
        assert_eq!(evaluate_auto_fix(&t, &enabled()), None);
        t.role = TaskRole::Mentioned;
        assert_eq!(evaluate_auto_fix(&t, &enabled()), None);
    }

    #[test]
    fn opt_out_label_skips_case_insensitive() {
        let mut t = pr();
        t.labels = vec![crate::Label::new("No-Auto-Fix")];
        assert_eq!(evaluate_auto_fix(&t, &enabled()), None);
        t.labels = vec![crate::Label::new("do-not-lazybox")];
        assert_eq!(evaluate_auto_fix(&t, &enabled()), None);
    }

    #[test]
    fn requested_reviewer_holds_off() {
        let mut t = pr();
        t.reviewers = vec!["alice".into()];
        assert_eq!(evaluate_auto_fix(&t, &enabled()), None);
    }

    #[test]
    fn changes_requested_holds_off() {
        let mut t = pr();
        t.review = ReviewStatus::ChangesRequested;
        assert_eq!(evaluate_auto_fix(&t, &enabled()), None);
    }

    #[test]
    fn merge_queue_holds_off() {
        let mut t = pr();
        t.is_in_merge_queue = true;
        assert_eq!(evaluate_auto_fix(&t, &enabled()), None);
        // ...even on a conflict.
        t.mergeable = Mergeable::new(crate::MergeableState::Conflicting, Utc::now());
        assert_eq!(evaluate_auto_fix(&t, &enabled()), None);
    }

    #[test]
    fn approved_pr_with_failing_ci_still_fixes() {
        // Approval doesn't mean a human is mid-review; an approved PR
        // whose CI then broke should still get fixed so it can merge.
        let mut t = pr();
        t.review = ReviewStatus::Approved;
        assert_eq!(
            evaluate_auto_fix(&t, &enabled()),
            Some(AutoFixKind::CiFailure)
        );
    }

    #[test]
    fn checks_field_does_not_affect_decision() {
        // The rolled-up `ci` field drives the decision; individual
        // `checks` are only used later to build the prompt.
        let mut t = pr();
        t.checks = vec![CheckRun {
            name: "build".into(),
            status: CiStatus::Failure,
            url: None,
        }];
        assert_eq!(
            evaluate_auto_fix(&t, &enabled()),
            Some(AutoFixKind::CiFailure)
        );
    }

    #[test]
    fn store_keys_are_distinct_and_stable() {
        assert_eq!(AutoFixKind::CiFailure.store_key(), "ci");
        assert_eq!(AutoFixKind::MergeConflict.store_key(), "conflict");
    }

    /// The task-shape half ignores the global switch and the label
    /// opt-out — it answers "is this PR's own state a fixable shape?"
    /// only. A label-opted-out PR is still a candidate here (the label
    /// is resolved by the policy layer in the dispatcher, issue #363).
    #[test]
    fn candidate_ignores_enable_and_label() {
        let mut t = pr();
        t.labels = vec![crate::Label::new("no-auto-fix")];
        // Global-disabled `evaluate_auto_fix` says no…
        assert_eq!(evaluate_auto_fix(&t, &AutoFixSettings::default()), None);
        // …but the pure candidate still sees a fixable CI failure.
        assert_eq!(auto_fix_candidate(&t), Some(AutoFixKind::CiFailure));
    }

    /// The label half is exactly the opt-out check, case-insensitive.
    #[test]
    fn opted_out_matches_labels_case_insensitively() {
        let mut t = pr();
        assert!(!is_auto_fix_opted_out(&t, &enabled()));
        t.labels = vec![crate::Label::new("Do-Not-Lazybox")];
        assert!(is_auto_fix_opted_out(&t, &enabled()));
    }

    /// `evaluate_auto_fix` == candidate gated by enable + !opted_out —
    /// the composition the split must preserve for the source path.
    #[test]
    fn evaluate_is_candidate_gated_by_enable_and_label() {
        let t = pr();
        assert_eq!(
            evaluate_auto_fix(&t, &enabled()),
            auto_fix_candidate(&t),
            "no label, enabled → same as candidate"
        );
        let mut labeled = pr();
        labeled.labels = vec![crate::Label::new("no-auto-fix")];
        assert_eq!(
            evaluate_auto_fix(&labeled, &enabled()),
            None,
            "label opt-out drops it in the composed fn"
        );
    }

    // ── resolve_auto_fix: the single composition ────────────────────

    /// `evaluate_auto_fix` is exactly the `Default`-arm shorthand of
    /// `resolve_auto_fix`. This is the invariant that lets the two-arg
    /// source path and the arm-aware path share one composition — if it
    /// ever breaks, the source path and the daemon have drifted.
    #[test]
    fn evaluate_is_resolve_with_default_arm() {
        for label in [None, Some("no-auto-fix"), Some("do-not-lazybox")] {
            for settings in [enabled(), AutoFixSettings::default()] {
                let mut t = pr();
                if let Some(l) = label {
                    t.labels = vec![crate::Label::new(l)];
                }
                assert_eq!(
                    evaluate_auto_fix(&t, &settings),
                    resolve_auto_fix(&t, &settings, PolicyArm::Default),
                    "evaluate must equal resolve(Default) for label={label:?} enabled={}",
                    settings.enabled,
                );
            }
        }
    }

    /// An explicit `Arm` overrides a label opt-out — the core promise of
    /// the per-session policy (issue #363). Same fixable PR that
    /// `evaluate_auto_fix` drops for its label is fixed once armed.
    #[test]
    fn arm_overrides_label_opt_out_at_resolve() {
        let mut t = pr();
        t.labels = vec![crate::Label::new("No-Auto-Fix")];
        // Default arm honors the label…
        assert_eq!(resolve_auto_fix(&t, &enabled(), PolicyArm::Default), None);
        // …an explicit Arm overrides it and fixes the CI failure.
        assert_eq!(
            resolve_auto_fix(&t, &enabled(), PolicyArm::Arm),
            Some(AutoFixKind::CiFailure)
        );
    }

    /// An explicit `Disarm` blocks even a clean, unlabeled candidate.
    #[test]
    fn disarm_blocks_clean_candidate_at_resolve() {
        let t = pr();
        assert!(!is_auto_fix_opted_out(&t, &enabled()));
        assert_eq!(
            resolve_auto_fix(&t, &enabled(), PolicyArm::Default),
            Some(AutoFixKind::CiFailure),
            "clean candidate fixes under Default"
        );
        assert_eq!(
            resolve_auto_fix(&t, &enabled(), PolicyArm::Disarm),
            None,
            "Disarm wins over a clean candidate"
        );
    }

    /// The global enable switch gates every arm — even an explicit `Arm`
    /// never fires while the feature is globally off.
    #[test]
    fn disabled_feature_beats_every_arm_at_resolve() {
        let t = pr();
        for arm in [PolicyArm::Default, PolicyArm::Arm, PolicyArm::Disarm] {
            assert_eq!(
                resolve_auto_fix(&t, &AutoFixSettings::default(), arm),
                None,
                "globally-disabled feature must not fire for arm={arm:?}",
            );
        }
    }

    /// `resolve_auto_fix` still requires a fixable task shape: a
    /// third-party or green PR is `None` under every arm, so an `Arm`
    /// can't conjure a fix out of a non-candidate.
    #[test]
    fn resolve_requires_candidate_shape_under_every_arm() {
        let mut green = pr();
        green.ci = CiStatus::Success;
        let mut third_party = pr();
        third_party.role = TaskRole::Reviewer;
        for arm in [PolicyArm::Default, PolicyArm::Arm, PolicyArm::Disarm] {
            assert_eq!(resolve_auto_fix(&green, &enabled(), arm), None);
            assert_eq!(resolve_auto_fix(&third_party, &enabled(), arm), None);
        }
    }

    // ── auto_fix_enabled_and_permitted: the daemon's gate ───────────

    /// The daemon dispatcher gate composes enable + label + arm exactly
    /// as `resolve_auto_fix` does. This table is the drift guard: if the
    /// server's gate and the pure path ever disagree, one of these fails.
    #[test]
    fn enabled_and_permitted_matches_resolve_gate() {
        // (enabled, label_opted_out, arm) → expected permit
        let cases = [
            // Disabled: never, regardless of label/arm.
            (false, false, PolicyArm::Default, false),
            (false, false, PolicyArm::Arm, false),
            (false, true, PolicyArm::Arm, false),
            (false, true, PolicyArm::Disarm, false),
            // Enabled + Default follows the label.
            (true, false, PolicyArm::Default, true),
            (true, true, PolicyArm::Default, false),
            // Enabled + Arm overrides a label opt-out.
            (true, true, PolicyArm::Arm, true),
            (true, false, PolicyArm::Arm, true),
            // Enabled + Disarm always off.
            (true, false, PolicyArm::Disarm, false),
            (true, true, PolicyArm::Disarm, false),
        ];
        for (enabled, opted_out, arm, expected) in cases {
            assert_eq!(
                auto_fix_enabled_and_permitted(enabled, opted_out, arm),
                expected,
                "enabled={enabled} opted_out={opted_out} arm={arm:?}",
            );
        }
    }

    /// The gate is precisely the `is_some()` of `resolve_auto_fix` for a
    /// shape-eligible candidate — proving the daemon's two-phase split
    /// (candidate at the source, gate at the dispatcher) reconstructs the
    /// same decision as the one-shot pure path.
    #[test]
    fn gate_reconstructs_resolve_for_a_candidate() {
        for enabled in [true, false] {
            for opted_out_label in [false, true] {
                for arm in [PolicyArm::Default, PolicyArm::Arm, PolicyArm::Disarm] {
                    let settings = AutoFixSettings {
                        enabled,
                        ..Default::default()
                    };
                    let mut t = pr(); // a CI-failing, own, open PR (a candidate)
                    if opted_out_label {
                        t.labels = vec![crate::Label::new("no-auto-fix")];
                    }
                    // The source phase confirms candidacy + computes the
                    // label; the dispatcher phase applies the gate.
                    let candidate = auto_fix_candidate(&t);
                    let label = is_auto_fix_opted_out(&t, &settings);
                    let two_phase = candidate
                        .filter(|_| auto_fix_enabled_and_permitted(settings.enabled, label, arm));
                    assert_eq!(
                        two_phase,
                        resolve_auto_fix(&t, &settings, arm),
                        "two-phase daemon path must equal one-shot resolve \
                         (enabled={enabled} label={opted_out_label} arm={arm:?})",
                    );
                }
            }
        }
    }
}
