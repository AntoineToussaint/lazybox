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
//! 1. **merge-on-green** — lazybox's arm
//!    ([`crate::Workspace::auto_merge_on_green`]). The *daemon* fires a
//!    merge the moment your own PR is merge-ready (see
//!    [`should_auto_merge`]), and only while the daemon runs. Its author
//!    gate defaults to your own PRs but opens to configured logins via
//!    [`MergeOnGreenPolicy`] (`merge_on_green.allow_authors`), so a green
//!    Dependabot bump can land automatically. It stays on its own
//!    `Workspace` field (already wired end-to-end) but is presented in
//!    the same policies surface.
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
//! - **native auto-merge > lazybox merge-on-green.** When GitHub's native
//!   auto-merge is already enabled on a PR, lazybox's merge-on-green
//!   stands down (see [`should_auto_merge`]) — GitHub will land it, so a
//!   second merge fired by lazybox is redundant and racy.
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

use crate::{AutoFixKind, Task, Workspace};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;

/// Which non-author logins may auto-merge their green PRs. The
/// merge-on-green author gate ([`auto_merge_block_reason`]) defaults to
/// "own PRs only"; this opens it, per config
/// (`merge_on_green.allow_authors`), so a green Dependabot/bot bump can
/// land automatically once opted in.
///
/// Logins are normalized on construction — lowercased and with a
/// trailing `[bot]` stripped — so `dependabot`, `Dependabot`, and
/// `dependabot[bot]` all match whichever form GitHub reports
/// (`author.login` is `dependabot` over GraphQL, `dependabot[bot]`
/// over REST). Empty (the default) preserves the own-PR-only guard for
/// everyone who doesn't opt in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MergeOnGreenPolicy {
    /// `None` opens the gate to every author — the internal "author
    /// check doesn't apply" mode the daemon's fire decision uses (the
    /// authoritative per-author gate runs in the merge attempt). `Some`
    /// carries the normalized allowlist.
    allow: Option<BTreeSet<String>>,
}

impl Default for MergeOnGreenPolicy {
    /// The safe default: own PRs only (an empty allowlist), NOT the
    /// gate-open [`MergeOnGreenPolicy::allow_all`]. A config-load
    /// failure or a missing `merge_on_green` section must never widen
    /// who can auto-merge.
    fn default() -> Self {
        Self {
            allow: Some(BTreeSet::new()),
        }
    }
}

/// Normalize a login for allowlist comparison: trim, lowercase, and
/// drop a trailing `[bot]` suffix.
fn normalize_author(login: &str) -> String {
    login.trim().trim_end_matches("[bot]").to_ascii_lowercase()
}

impl MergeOnGreenPolicy {
    /// Build from a configured allowlist of logins.
    pub fn from_allow_authors<I, S>(authors: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        let allow = authors
            .into_iter()
            .map(|a| normalize_author(a.as_ref()))
            .filter(|a| !a.is_empty())
            .collect();
        Self { allow: Some(allow) }
    }

    /// The gate-open policy: every author is allowed. Used by the
    /// daemon's fire decision so any otherwise-mergeable green PR
    /// dispatches an attempt; the attempt then re-checks against the
    /// real configured policy. Keeps the fire path free of config I/O.
    pub fn allow_all() -> Self {
        Self { allow: None }
    }

    /// Is a non-author `login` permitted to auto-merge? An empty login
    /// never matches (a legacy snapshot with no author can't be opted
    /// in by mistake).
    pub fn allows_author(&self, login: &str) -> bool {
        match &self.allow {
            None => true,
            Some(set) => {
                let norm = normalize_author(login);
                !norm.is_empty() && set.contains(&norm)
            }
        }
    }
}

/// User-facing phrase for the merge-on-green author gate. Shared so the
/// daemon's arm-time refusal, the merge attempt's stand-down, and
/// [`auto_merge_block_reason`] all name the block identically.
pub const NON_AUTHOR_BLOCK: &str = "only your own PRs auto-merge";

/// What a repository requires before lazybox treats a PR's approval as
/// satisfying its READY / merge-on-green gate. Configured per repo via
/// `repos.<owner/name>.approval` and carried on [`crate::Task::approval_policy`]
/// so the review gate — which runs in the config-less TUI/core layer —
/// can honor it without a config lookup.
///
/// The policy only ever makes the gate **stricter**. GitHub's own
/// review-decision already counts an eligible bot's approval, and a
/// branch-protection rule a bot can't satisfy surfaces as
/// [`crate::Task::merge_blocked`] (issue #998). So `Default` (and the
/// explicit `bot-ok` / `any` config values, which map to it) impose
/// nothing beyond that — only `Human` adds a requirement, so a bot-only
/// approval from a reviewer like `claude[bot]` no longer reads as ready.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[cfg_attr(feature = "desktop-contract", derive(ts_rs::TS))]
pub enum ApprovalPolicy {
    /// Follow GitHub's review-decision; a bot approval counts. The
    /// default, and what the `bot-ok` / `any` config values resolve to.
    #[default]
    Default,
    /// A human approval is required — a bot-only (or absent) approval
    /// does not satisfy READY or arm merge-on-green.
    Human,
}

/// Why this repo's approval policy holds a PR back from READY /
/// merge-on-green — or `None` when the policy is satisfied. Only the
/// [`ApprovalPolicy::Human`] policy can block: it requires a submitted
/// human approval ([`crate::Task::has_human_approval`]), so a PR that
/// GitHub reports as approved purely on a bot's review stays "not yet".
pub fn approval_policy_blocks(pr: &Task) -> Option<&'static str> {
    match pr.approval_policy {
        ApprovalPolicy::Default => None,
        ApprovalPolicy::Human if pr.has_human_approval() => None,
        ApprovalPolicy::Human => Some("a human approval is required by this repo's policy"),
    }
}

/// Does the merge-on-green **author gate** alone block this PR — it isn't
/// yours ([`crate::TaskRole::Author`]) and its author isn't opted in via
/// `policy`? Unlike the CI / review / conflict guards, this is *stable*
/// across a poll: a PR's author never changes, nor does your relationship
/// to it. That lets callers decide it against a stored snapshot — the
/// daemon refuses the arm up front on this, and the merge attempt
/// short-circuits on it before spending an API call on a PR that can
/// never merge.
pub fn author_gate_blocks(pr: &Task, policy: &MergeOnGreenPolicy) -> bool {
    pr.role != crate::TaskRole::Author && !policy.allows_author(&pr.author)
}

/// Why this PR can't be merged from lazybox right now, as a
/// user-facing phrase — or `None` when nothing blocks it. Drives both
/// the merge-ready gate (`resolve_merge` in `lazybox-tui-core`) and the
/// message shown when a confirmed merge can't proceed, so the hint and
/// the explanation never disagree. Lives in core (not the TUI layer)
/// because the daemon's auto-merge path re-verifies eligibility with
/// the same predicate before every automatic merge.
///
/// We deliberately do NOT require a formal `Approved` review by default.
/// Repos without required reviews — a personal repo, your own PR — let
/// you merge with no approval, so demanding one here produced false
/// "not merge-ready" blocks. We only veto on `ChangesRequested`, which
/// is an unambiguous "not yet" regardless of branch protection — and,
/// when a repo opts into [`ApprovalPolicy::Human`], on a bot-only
/// approval that doesn't meet that stricter policy
/// ([`approval_policy_blocks`]).
///
/// We DO veto when GitHub itself reports the merge blocked by a
/// branch-protection rule or repository ruleset ([`Task::merge_blocked`],
/// from `mergeStateStatus == BLOCKED`): a required review, check,
/// signature, or linear-history rule is unsatisfied, so the merge would
/// bounce with "Repository rule violations found" (issue #998). This is
/// GitHub's own computed verdict — not a guess at rules we don't fetch —
/// so gating on it keeps lazybox from offering (or tagging READY) a merge
/// it can't land. A racing/stale `BLOCKED` that slips through still hits
/// GitHub's rejection, which the server humanizes.
pub fn merge_block_reason(pr: &Task) -> Option<&'static str> {
    if !matches!(
        pr.state,
        crate::TaskState::Open | crate::TaskState::InReview
    ) {
        return Some("the PR isn't open");
    }
    if matches!(pr.review, crate::ReviewStatus::ChangesRequested) {
        return Some("changes were requested — address the review first");
    }
    if let Some(reason) = approval_policy_blocks(pr) {
        return Some(reason);
    }
    if !matches!(pr.ci, crate::CiStatus::Success | crate::CiStatus::None) {
        return Some("CI isn't green yet");
    }
    if pr.mergeable.is_conflicting() {
        return Some("the branch has merge conflicts");
    }
    if pr.merge_blocked {
        return Some(
            "a repository rule blocks the merge — required reviews, checks, or signatures aren't satisfied",
        );
    }
    None
}

/// Why "auto-merge on green" must NOT fire for this workspace *right
/// now*, as a user-facing phrase — or `None` when an automatic merge is
/// allowed. [`should_auto_merge`] is the boolean view of this.
///
/// The auto gate is deliberately **stricter** than [`merge_block_reason`]
/// (the manual `g m` gate), because auto-merge acts with no keypress:
///
/// 1. The workspace must be armed (`auto_merge_on_green`).
/// 2. Your **own** PR only (`TaskRole::Author`), unless the author is
///    opted in via `policy` (`merge_on_green.allow_authors`) — lazybox
///    never auto-merges an arbitrary third party's PR, even if green,
///    but a configured Dependabot/bot author may.
/// 3. CI must be positively **green** (`CiStatus::Success`). Unlike the
///    manual gate, `CiStatus::None` (a PR with no checks configured)
///    does NOT qualify — we don't silently land a PR that never ran CI.
/// 4. Everything the manual gate blocks on still blocks
///    ([`merge_block_reason`]): the PR must be open (drafts, merged and
///    closed PRs are out), no changes-requested review, no conflict.
/// 5. GitHub's **native** auto-merge is not already enabled. Precedence
///    (issue #363): native auto-merge wins — GitHub will land the PR
///    itself once it's ready, so firing lazybox's own merge on top is
///    redundant and races the server-side merge.
///
/// Nothing here that `g m` wouldn't also merge — this is a subset.
pub fn auto_merge_block_reason(
    workspace: &Workspace,
    policy: &MergeOnGreenPolicy,
) -> Option<&'static str> {
    if !workspace.auto_merge_on_green {
        return Some("auto-merge on green isn't armed");
    }
    let Some(pr) = workspace.pr.as_ref() else {
        return Some("the workspace has no PR");
    };
    // Native auto-merge takes precedence — let GitHub land it.
    if pr.auto_merge_enabled {
        return Some("GitHub's native auto-merge is already enabled");
    }
    if author_gate_blocks(pr, policy) {
        return Some(NON_AUTHOR_BLOCK);
    }
    if pr.ci != crate::CiStatus::Success {
        return Some("CI isn't green");
    }
    merge_block_reason(pr)
}

/// Should the daemon auto-fire a merge for this workspace *right now*?
///
/// The "auto-merge on green" trigger — evaluated by the daemon's
/// polling commit path (so it fires with or without an attached TUI,
/// and exactly once across any number of clients). See
/// [`auto_merge_block_reason`] for the full guard list.
pub fn should_auto_merge(workspace: &Workspace, policy: &MergeOnGreenPolicy) -> bool {
    auto_merge_block_reason(workspace, policy).is_none()
}

/// Per-session override for an auto-fix behavior. The unified,
/// discoverable replacement for "label opt-out only" — a workspace can
/// now positively arm or explicitly disarm auto-fix for itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[cfg_attr(feature = "desktop-contract", derive(ts_rs::TS))]
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

    /// The more decisive of two arms when merging policy across a workspace
    /// transfer: an explicit `Disarm` outranks an explicit `Arm`, which
    /// outranks the implicit `Default`. Mirrors the precedence in
    /// [`auto_fix_permitted`] (an explicit "off" beats all).
    fn max_decided(self, other: PolicyArm) -> PolicyArm {
        fn rank(a: PolicyArm) -> u8 {
            match a {
                PolicyArm::Default => 0,
                PolicyArm::Arm => 1,
                PolicyArm::Disarm => 2,
            }
        }
        if rank(other) > rank(self) {
            other
        } else {
            self
        }
    }
}

/// Per-workspace automation-policy set. Persisted on [`crate::Workspace`]
/// (serde-defaulted, so pre-#363 records read back as all-`Default` and
/// behave unchanged).
///
/// **Scope, deliberately:** this struct owns only the per-session
/// auto-fix arms. It is *not* the whole automation model — `merge_on_green`
/// stays on its own `Workspace::auto_merge_on_green` field and
/// GitHub-native auto-merge stays on `Task::auto_merge_enabled`. Those two
/// were already wired end-to-end and persisted; folding them in here would
/// mean migrating live user state for no behavioral gain. The *unification*
/// #363 asked for is at the **surface** — one menu and one pill vocabulary
/// present all three together (see the module docs) — not a single storage
/// field. Keep new per-session, lazybox-owned policies here; leave
/// GitHub-owned server state on `Task`.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[cfg_attr(feature = "desktop-contract", derive(ts_rs::TS))]
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

    /// Fold another policy set into this one, per arm, keeping the more
    /// decisive choice (`Disarm` > `Arm` > `Default`). Used when a session
    /// moves between workspaces (issue #554) so an explicit arm/disarm the
    /// user set on the source survives the transfer.
    ///
    /// The exhaustive destructure is deliberate: a new policy arm added to
    /// [`AutomationPolicies`] is a compile error here until it's given a
    /// merge rule, so workspace state can't silently fail to transfer.
    pub fn absorb_from(&mut self, other: &AutomationPolicies) {
        let AutomationPolicies {
            auto_fix_ci,
            auto_fix_conflict,
        } = other;
        self.auto_fix_ci = self.auto_fix_ci.max_decided(*auto_fix_ci);
        self.auto_fix_conflict = self.auto_fix_conflict.max_decided(*auto_fix_conflict);
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

/// The next `PolicyArm` in the policies-menu toggle cycle:
/// `Default → Disarm → Arm → Default`.
///
/// Deliberately **label-agnostic** — it depends only on the current arm,
/// never on whether a label opts the PR out. The daemon holds the
/// authoritative opt-out set, so a client that can't see it (e.g. a
/// remote `--connect` session whose local config differs) still produces
/// a correct, unambiguous next state: each variant means the same thing
/// on both sides regardless of labels. From the common `Default` a
/// single press `Disarm`s (the usual "make auto-fix stop here"); a
/// second `Arm`s (force on, overriding any opt-out label); a third
/// returns to `Default`.
pub fn toggled_arm(arm: PolicyArm) -> PolicyArm {
    match arm {
        PolicyArm::Default => PolicyArm::Disarm,
        PolicyArm::Disarm => PolicyArm::Arm,
        PolicyArm::Arm => PolicyArm::Default,
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

    /// Toggling walks a fixed, label-agnostic cycle so the same keypress
    /// produces the same next state on any client, regardless of what it
    /// knows about the PR's labels.
    #[test]
    fn toggle_cycles_default_disarm_arm() {
        assert_eq!(toggled_arm(PolicyArm::Default), PolicyArm::Disarm);
        assert_eq!(toggled_arm(PolicyArm::Disarm), PolicyArm::Arm);
        assert_eq!(toggled_arm(PolicyArm::Arm), PolicyArm::Default);
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

    /// Merging policies across a workspace transfer keeps the more
    /// decisive arm per kind: `Disarm` > `Arm` > `Default` (issue #554).
    #[test]
    fn absorb_from_keeps_most_decisive_arm() {
        // Disarm on either side wins over Arm.
        let mut a = AutomationPolicies::default();
        a.set(AutoFixKind::CiFailure, PolicyArm::Arm);
        let mut b = AutomationPolicies::default();
        b.set(AutoFixKind::CiFailure, PolicyArm::Disarm);
        a.absorb_from(&b);
        assert_eq!(a.arm(AutoFixKind::CiFailure), PolicyArm::Disarm);

        // An explicit arm on the source lands on a Default destination.
        let mut c = AutomationPolicies::default();
        let mut d = AutomationPolicies::default();
        d.set(AutoFixKind::MergeConflict, PolicyArm::Arm);
        c.absorb_from(&d);
        assert_eq!(c.arm(AutoFixKind::MergeConflict), PolicyArm::Arm);

        // A Default source never downgrades an armed destination.
        let mut e = AutomationPolicies::default();
        e.set(AutoFixKind::CiFailure, PolicyArm::Arm);
        e.absorb_from(&AutomationPolicies::default());
        assert_eq!(e.arm(AutoFixKind::CiFailure), PolicyArm::Arm);
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

/// Merge-eligibility tests, moved here from `lazybox-tui-core`'s
/// `intent` module when the predicate moved into core so the daemon's
/// auto-merge path could share it.
#[cfg(test)]
mod merge_gate_tests {
    use super::*;
    use crate::{CiStatus, ReviewStatus, Task, TaskId, TaskRole, TaskState, Workspace};
    use chrono::Utc;

    fn pr(key: &str, ci: CiStatus, review: ReviewStatus) -> Workspace {
        let (path, num) = key.rsplit_once('#').unwrap_or((key, "1"));
        let task = Task {
            author: String::new(),
            id: TaskId {
                source: "github".into(),
                key: key.into(),
            },
            title: format!("PR {key}"),
            body: None,
            state: TaskState::Open,
            role: TaskRole::Author,
            ci,
            review,
            checks: vec![],
            unread_count: 0,
            url: format!("https://github.com/{path}/pull/{num}"),
            repo: Some("o/r".into()),
            branch: Some("main".into()),
            base_branch: None,
            updated_at: Utc::now(),
            created_at: None,
            closed_at: None,
            labels: vec![],
            reviewers: vec![],
            reviews: vec![],
            assignees: vec![],
            auto_merge_enabled: false,
            is_in_merge_queue: false,
            mergeable: crate::Mergeable::Mergeable,
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
            priority: None,
            state_label: None,
        };
        Workspace::from_task(task, Utc::now())
    }

    fn issue(key: &str) -> Workspace {
        let (path, num) = key.rsplit_once('#').unwrap_or((key, "1"));
        let mut t = pr(key, CiStatus::None, ReviewStatus::None);
        // Convert to issue: clear pr, attach as gh_issue.
        let mut task = t.pr.take().unwrap();
        task.url = format!("https://github.com/{path}/issues/{num}");
        t.attach_task(task);
        t
    }

    /// The default (own-PRs-only) policy: no author is allowlisted.
    fn own() -> MergeOnGreenPolicy {
        MergeOnGreenPolicy::default()
    }

    #[test]
    fn should_auto_merge_only_when_armed() {
        let mut ws = pr("o/r#1", CiStatus::Success, ReviewStatus::Approved);
        assert!(
            !should_auto_merge(&ws, &own()),
            "disarmed workspace never fires"
        );
        ws.auto_merge_on_green = true;
        assert!(
            should_auto_merge(&ws, &own()),
            "armed + green + own PR fires"
        );
    }

    #[test]
    fn should_auto_merge_requires_own_pr() {
        // Never auto-merge someone else's PR, even armed + green.
        for role in [TaskRole::Reviewer, TaskRole::Assignee, TaskRole::Mentioned] {
            let mut ws = pr("o/r#1", CiStatus::Success, ReviewStatus::Approved);
            ws.pr.as_mut().unwrap().role = role;
            ws.auto_merge_on_green = true;
            assert!(
                !should_auto_merge(&ws, &own()),
                "role {role:?} must not auto-merge"
            );
        }
    }

    #[test]
    fn should_auto_merge_requires_green_not_none() {
        // Manual `g m` treats CiStatus::None as mergeable, but
        // auto-merge must NOT land a PR that never ran CI.
        let mut ws = pr("o/r#1", CiStatus::None, ReviewStatus::Approved);
        ws.auto_merge_on_green = true;
        assert!(
            !should_auto_merge(&ws, &own()),
            "no-CI PR must not auto-merge even though `g m` would allow it"
        );
        // Sanity: the manual gate DOES allow this one.
        assert_eq!(merge_block_reason(ws.pr.as_ref().unwrap()), None);
    }

    #[test]
    fn should_auto_merge_honors_every_manual_block() {
        // Anything the manual gate blocks, auto-merge blocks too.
        let mut armed_conflict = pr("o/r#1", CiStatus::Success, ReviewStatus::None);
        armed_conflict.auto_merge_on_green = true;
        armed_conflict.pr.as_mut().unwrap().mergeable = crate::Mergeable::Conflicting;
        assert!(
            !should_auto_merge(&armed_conflict, &own()),
            "conflict blocks"
        );

        let mut armed_changes = pr("o/r#1", CiStatus::Success, ReviewStatus::ChangesRequested);
        armed_changes.auto_merge_on_green = true;
        assert!(
            !should_auto_merge(&armed_changes, &own()),
            "changes-requested blocks"
        );

        let mut armed_failing = pr("o/r#1", CiStatus::Failure, ReviewStatus::Approved);
        armed_failing.auto_merge_on_green = true;
        assert!(!should_auto_merge(&armed_failing, &own()), "red CI blocks");
    }

    #[test]
    fn should_auto_merge_defers_to_native_auto_merge() {
        // Precedence (issue #363): when GitHub's native auto-merge is
        // already enabled, lazybox's merge-on-green stands down —
        // GitHub will land it, so a second lazybox merge is redundant
        // and races the server-side one.
        let mut ws = pr("o/r#1", CiStatus::Success, ReviewStatus::Approved);
        ws.auto_merge_on_green = true;
        assert!(
            should_auto_merge(&ws, &own()),
            "armed + green fires without native"
        );
        ws.pr.as_mut().unwrap().auto_merge_enabled = true;
        assert!(
            !should_auto_merge(&ws, &own()),
            "native auto-merge takes precedence over lazybox merge-on-green"
        );
    }

    #[test]
    fn should_auto_merge_never_fires_on_draft() {
        let mut ws = pr("o/r#1", CiStatus::Success, ReviewStatus::Approved);
        ws.auto_merge_on_green = true;
        ws.pr.as_mut().unwrap().state = TaskState::Draft;
        assert!(
            !should_auto_merge(&ws, &own()),
            "draft PRs never auto-merge"
        );
    }

    #[test]
    fn should_auto_merge_never_fires_on_issue() {
        let mut ws = issue("o/r#42");
        ws.auto_merge_on_green = true;
        assert!(
            !should_auto_merge(&ws, &own()),
            "issue workspaces have no PR to merge"
        );
    }

    /// The reason view and the boolean view can never disagree — the
    /// daemon's stand-down notice names the same guard the predicate
    /// tripped on.
    #[test]
    fn block_reason_is_none_exactly_when_predicate_fires() {
        let mut ws = pr("o/r#1", CiStatus::Success, ReviewStatus::Approved);
        ws.auto_merge_on_green = true;
        assert_eq!(auto_merge_block_reason(&ws, &own()), None);
        ws.pr.as_mut().unwrap().ci = CiStatus::Failure;
        assert_eq!(auto_merge_block_reason(&ws, &own()), Some("CI isn't green"));
        ws.pr.as_mut().unwrap().ci = CiStatus::Success;
        ws.pr.as_mut().unwrap().review = ReviewStatus::ChangesRequested;
        assert_eq!(
            auto_merge_block_reason(&ws, &own()),
            Some("changes were requested — address the review first")
        );
    }

    /// A non-author PR blocks under the default policy but merges once
    /// its author is opted in via `merge_on_green.allow_authors` — the
    /// Dependabot use case (issue #845).
    #[test]
    fn allowlisted_author_auto_merges_a_non_own_pr() {
        let mut ws = pr("o/r#1", CiStatus::Success, ReviewStatus::Approved);
        ws.auto_merge_on_green = true;
        ws.pr.as_mut().unwrap().role = TaskRole::Mentioned;
        ws.pr.as_mut().unwrap().author = "dependabot[bot]".into();

        // Default: own PRs only — the bot PR stands down with a reason.
        assert_eq!(
            auto_merge_block_reason(&ws, &own()),
            Some("only your own PRs auto-merge")
        );

        // Opted in (config login `dependabot`, PR author `dependabot[bot]`
        // — the `[bot]` suffix and case are normalized away).
        let allowed = MergeOnGreenPolicy::from_allow_authors(["Dependabot"]);
        assert!(
            should_auto_merge(&ws, &allowed),
            "a green PR from an allowlisted author auto-merges"
        );

        // A different bot is still blocked — the allowlist is exact.
        ws.pr.as_mut().unwrap().author = "renovate[bot]".into();
        assert_eq!(
            auto_merge_block_reason(&ws, &allowed),
            Some("only your own PRs auto-merge")
        );
    }

    /// `allow_all` opens the author gate (the daemon's fire decision)
    /// but leaves every other guard — CI, conflict, review — intact.
    #[test]
    fn allow_all_opens_only_the_author_gate() {
        let mut ws = pr("o/r#1", CiStatus::Success, ReviewStatus::Approved);
        ws.auto_merge_on_green = true;
        ws.pr.as_mut().unwrap().role = TaskRole::Reviewer;
        assert!(
            should_auto_merge(&ws, &MergeOnGreenPolicy::allow_all()),
            "author-agnostic policy fires a green non-own PR"
        );
        // But a red one still blocks.
        ws.pr.as_mut().unwrap().ci = CiStatus::Failure;
        assert_eq!(
            auto_merge_block_reason(&ws, &MergeOnGreenPolicy::allow_all()),
            Some("CI isn't green")
        );
    }

    /// Issue #998: a PR GitHub reports as ruleset-blocked
    /// (`merge_blocked`, from `mergeStateStatus == BLOCKED`) is not
    /// merge-ready even when approved + green, so `g m` / READY stand
    /// down instead of offering a merge that bounces. Auto-merge inherits
    /// the same veto (it delegates to `merge_block_reason`).
    #[test]
    fn ruleset_block_is_not_merge_ready() {
        let mut ws = pr("o/r#1", CiStatus::Success, ReviewStatus::Approved);
        assert_eq!(
            merge_block_reason(ws.pr.as_ref().unwrap()),
            None,
            "approved + green merges when no rule blocks it"
        );

        ws.pr.as_mut().unwrap().merge_blocked = true;
        assert!(
            merge_block_reason(ws.pr.as_ref().unwrap()).is_some(),
            "a ruleset-blocked PR is not merge-ready"
        );

        // And auto-merge stands down on it too.
        ws.auto_merge_on_green = true;
        assert!(
            !should_auto_merge(&ws, &own()),
            "auto-merge never fires on a ruleset-blocked PR"
        );
    }

    /// An empty author never matches an allowlist — a legacy snapshot
    /// with no recorded author can't be opted in by accident.
    #[test]
    fn empty_author_never_allowlisted() {
        let allowed = MergeOnGreenPolicy::from_allow_authors(["dependabot"]);
        assert!(!allowed.allows_author(""));
        assert!(!allowed.allows_author("   "));
    }

    fn reviewer(login: &str, state: crate::ReviewState, is_bot: bool) -> crate::Reviewer {
        crate::Reviewer {
            login: login.into(),
            state,
            is_bot,
        }
    }

    /// Issue #1048: under `approval: human`, a bot-only approval is NOT
    /// merge-ready, but the same PR IS ready under the default (bot-ok)
    /// policy — GitHub's review-decision already counted the bot.
    #[test]
    fn bot_only_approval_ready_under_default_not_human() {
        let mut ws = pr("o/r#1", CiStatus::Success, ReviewStatus::Approved);
        ws.pr.as_mut().unwrap().reviews =
            vec![reviewer("claude", crate::ReviewState::Approved, true)];

        // Default policy: the bot approval satisfies the gate.
        ws.pr.as_mut().unwrap().approval_policy = ApprovalPolicy::Default;
        assert_eq!(merge_block_reason(ws.pr.as_ref().unwrap()), None);

        // Human policy: a bot-only approval no longer satisfies it.
        ws.pr.as_mut().unwrap().approval_policy = ApprovalPolicy::Human;
        assert_eq!(
            merge_block_reason(ws.pr.as_ref().unwrap()),
            Some("a human approval is required by this repo's policy")
        );
    }

    /// A human approval satisfies the `human` policy even when a bot also
    /// approved.
    #[test]
    fn human_approval_satisfies_human_policy() {
        let mut ws = pr("o/r#1", CiStatus::Success, ReviewStatus::Approved);
        ws.pr.as_mut().unwrap().approval_policy = ApprovalPolicy::Human;
        ws.pr.as_mut().unwrap().reviews = vec![
            reviewer("claude", crate::ReviewState::Approved, true),
            reviewer("alice", crate::ReviewState::Approved, false),
        ];
        assert!(ws.pr.as_ref().unwrap().has_human_approval());
        assert_eq!(merge_block_reason(ws.pr.as_ref().unwrap()), None);
    }

    /// Auto-merge inherits the per-repo approval gate (it delegates to
    /// [`merge_block_reason`]): an armed, green, own PR with only a bot
    /// approval must not auto-merge under `human`, but does once a human
    /// approves.
    #[test]
    fn auto_merge_honors_human_approval_policy() {
        let mut ws = pr("o/r#1", CiStatus::Success, ReviewStatus::Approved);
        ws.auto_merge_on_green = true;
        ws.pr.as_mut().unwrap().approval_policy = ApprovalPolicy::Human;
        ws.pr.as_mut().unwrap().reviews =
            vec![reviewer("claude", crate::ReviewState::Approved, true)];
        assert!(
            !should_auto_merge(&ws, &own()),
            "bot-only approval must not auto-merge under `human`"
        );

        ws.pr.as_mut().unwrap().reviews.push(reviewer(
            "alice",
            crate::ReviewState::Approved,
            false,
        ));
        assert!(
            should_auto_merge(&ws, &own()),
            "a human approval arms the merge"
        );
    }

    /// The default policy imposes no approval requirement at all — an own
    /// PR with no approval still merges (the pre-#1048 behavior).
    #[test]
    fn default_policy_requires_no_approval() {
        let ws = pr("o/r#1", CiStatus::Success, ReviewStatus::None);
        assert_eq!(
            ws.pr.as_ref().unwrap().approval_policy,
            ApprovalPolicy::Default
        );
        assert_eq!(merge_block_reason(ws.pr.as_ref().unwrap()), None);
    }
}
