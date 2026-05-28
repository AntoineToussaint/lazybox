//! Auto-inject fix work on CI failure or merge conflict.
//!
//! When a PR pilot tracks fails CI or develops a merge conflict, the
//! next step — spawn an agent, point it at the failure, push a fix —
//! is mechanical. This module holds the *pure* decision layer: given a
//! [`Task`] and the user's [`AutoFixSettings`], should pilot kick off
//! an auto-fix, and of which kind? The stateful pieces (attempt
//! counting, cooldown, posting the PR comment, spawning the agent)
//! live in `pilot-server`'s polling layer, which calls
//! [`evaluate_auto_fix`] and then layers the guards that need the
//! store on top.
//!
//! ## Why the guards live here (and not in the server)
//!
//! The "should we touch this PR at all" predicate is the part most
//! worth unit-testing exhaustively — it's the difference between
//! pilot quietly fixing your own broken build and pilot rewriting a
//! teammate's PR uninvited. Keeping it pure (no store, no network)
//! means every guard gets a fast deterministic test, and the server
//! layer only has to test the stateful parts (cooldown / max-attempts)
//! once.

use crate::{CiStatus, ReviewStatus, Task, TaskRole, TaskState};
use std::time::Duration;

/// Which failure mode triggered an auto-fix. Determines the prompt
/// the agent gets and the key the attempt counter is tracked under
/// (CI failures and merge conflicts get *separate* attempt budgets —
/// a flaky test shouldn't burn the conflict budget and vice versa).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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
/// `~/.pilot/config.yaml` (`auto_fix:` block). Lives in `pilot-core`
/// (not `pilot-config`) so the pure guard and the server layer share
/// one type without `pilot-config` leaking into either; `pilot-config`
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
            opt_out_labels: vec!["no-auto-fix".into(), "do-not-pilot".into()],
            max_attempts: 3,
            cooldown: Duration::from_secs(60 * 60),
            window: Duration::from_secs(24 * 60 * 60),
        }
    }
}

/// Decide whether `task` warrants an auto-fix right now, and of which
/// kind. Pure — no store, no clock. Returns `None` when any guard
/// blocks; the server layer adds the stateful guards (cooldown,
/// max-attempts) on top of a `Some` result.
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
///    hold off so pilot isn't force-pushing under them.
/// 7. **Merge queue** — not currently in GitHub's merge queue; let the
///    queue resolve.
///
/// Trigger priority: a conflict outranks a CI failure (you can't get
/// clean CI on a branch that won't even merge). When both are present
/// we resolve the conflict first; the next sweep picks up any
/// remaining CI failure.
pub fn evaluate_auto_fix(task: &Task, settings: &AutoFixSettings) -> Option<AutoFixKind> {
    if !settings.enabled {
        return None;
    }
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
    // Opt-out label (case-insensitive).
    let opted_out = task.labels.iter().any(|label| {
        settings
            .opt_out_labels
            .iter()
            .any(|opt| opt.eq_ignore_ascii_case(&label.name))
    });
    if opted_out {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{CheckRun, Mergeable, ReviewStatus, TaskId};
    use chrono::Utc;

    fn pr() -> Task {
        Task {
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
            labels: vec![],
            reviewers: vec![],
            assignees: vec![],
            auto_merge_enabled: false,
            is_in_merge_queue: false,
            mergeable: Mergeable::Mergeable,
            is_behind_base: false,
            node_id: None,
            needs_reply: false,
            last_commenter: None,
            recent_activity: vec![],
            additions: 0,
            deletions: 0,
            closes_issues: vec![],
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
        t.mergeable = Mergeable::Conflicting;
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
        t.mergeable = Mergeable::Mergeable;
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
        t.mergeable = crate::Mergeable::Conflicting;
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
        t.labels = vec![crate::Label::new("do-not-pilot")];
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
        t.mergeable = Mergeable::Conflicting;
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
}
