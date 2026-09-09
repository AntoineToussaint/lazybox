//! Model-**capability** tier a task declares, and the pure resolver that
//! reads it off a [`Task`].
//!
//! A task can request a right-sized model by declaring a capability
//! tier: a `best` / `high` / `medium` / `low` **label**, or an `@best` /
//! `@high` / `@medium` / `@low` **marker** in its body. The autonomous
//! ("pilot") spawn paths and a bare interactive spawn resolve the tier
//! here; the spawn path then maps it to one of the target agent's
//! model-tier aliases
//! ([`AgentModels::alias_for_capability`](crate::AgentModels::alias_for_capability))
//! and appends that tier's model args. Best → the strongest available
//! run (model *and* reasoning effort), low → the cheapest/fastest.
//!
//! **This is not a priority and not a ranking.** Choosing the model is
//! the *only* effect: nothing here ranks, queues, orders, or schedules
//! work. Lazybox's genuine priority field is
//! [`Priority`](crate::Priority) on [`Task`], which carries Linear's
//! ranking — a different type with a different meaning that happens to
//! share the word `High` (#1598).
//!
//! This module holds only the pure decision (tier ← task); the
//! tier → tier-alias → concrete-model mapping lives on
//! [`AgentModels`](crate::AgentModels), and the injection lives in the
//! agent spawn path.

use crate::Task;

/// The model-capability tier a task declares, mapped by the spawn path
/// onto one of the target agent's model tiers. Not a priority: it picks
/// which model runs the task, never when or in what order (#1598).
///
/// Ordered strongest → cheapest so the resolver can prefer the higher
/// tier when a task somehow declares more than one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CapabilityTier {
    Best,
    High,
    Medium,
    Low,
}

impl CapabilityTier {
    /// Strongest → cheapest. The resolver scans in this order so a task
    /// carrying, say, both a `best` and a `high` label resolves to
    /// `Best` (the stronger wins).
    const ORDER: [CapabilityTier; 4] = [Self::Best, Self::High, Self::Medium, Self::Low];

    /// Lowercase token this tier is declared with — the label name and
    /// the `@`-marker suffix (`best` / `high` / `medium` / `low`).
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Best => "best",
            Self::High => "high",
            Self::Medium => "medium",
            Self::Low => "low",
        }
    }
}

/// Resolve the [`CapabilityTier`] a task declares, if any.
///
/// Precedence:
/// 1. A `best` / `high` / `medium` / `low` **label** (case-insensitive)
///    wins over a body marker.
/// 2. Otherwise a `@best` / `@high` / `@medium` / `@low` **marker** in
///    the body, matched at a word boundary (so `@highest` / `email@high`
///    don't count).
///
/// When a source declares more than one tier (two tier labels, or two
/// markers), the stronger tier wins — `CapabilityTier::ORDER` is
/// scanned strongest-first.
///
/// Returns `None` when nothing is declared; the caller falls back to
/// the agent's configured default tier.
pub fn resolve_capability_tier(task: &Task) -> Option<CapabilityTier> {
    tier_from_labels(task).or_else(|| tier_from_body(task.body.as_deref().unwrap_or("")))
}

fn tier_from_labels(task: &Task) -> Option<CapabilityTier> {
    CapabilityTier::ORDER.into_iter().find(|tier| {
        task.labels
            .iter()
            .any(|label| label.name.eq_ignore_ascii_case(tier.as_str()))
    })
}

fn tier_from_body(body: &str) -> Option<CapabilityTier> {
    CapabilityTier::ORDER
        .into_iter()
        .find(|tier| contains_at_marker(body, tier.as_str()))
}

/// True when `body` contains `@<word>` at a word boundary,
/// case-insensitive on `word`. Mirrors the `@lazybox` boundary logic in
/// `gh-provider`'s `mentions` module (kept here because `lazybox-core`
/// can't depend on a provider crate): the `@` must not be preceded by a
/// login char or another `@`, and the char after the word must not
/// continue a login/handle (`@highest`, `@high-1`, `@high.io`,
/// `foo@high` all miss).
fn contains_at_marker(body: &str, word: &str) -> bool {
    let bytes = body.as_bytes();
    // Needle is `@word`, e.g. `@high`.
    let needle: Vec<u8> = std::iter::once(b'@').chain(word.bytes()).collect();
    let n = needle.len();
    if bytes.len() < n {
        return false;
    }
    for i in 0..=bytes.len() - n {
        let window = &bytes[i..i + n];
        if !window
            .iter()
            .zip(needle.iter())
            .all(|(a, b)| a.eq_ignore_ascii_case(b))
        {
            continue;
        }
        // Pre-boundary: `@` must not be glued to an identifier char.
        if i > 0 {
            let prev = bytes[i - 1];
            if is_login_char(prev) || prev == b'@' {
                continue;
            }
        }
        // Post-boundary: the char after the word must not continue a
        // login/handle (`.` rejected to skip `@high.io` email-likes).
        if let Some(&next) = bytes.get(i + n)
            && (is_login_char(next) || next == b'.' || next == b'@')
        {
            continue;
        }
        return true;
    }
    false
}

/// GitHub login alphabet used for the word-boundary check: ASCII
/// alphanumerics plus hyphen and underscore. Same alphabet the
/// `@lazybox` mention detector uses.
fn is_login_char(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'-' || b == b'_'
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Label, Task, TaskId, TaskRole, TaskState};
    use chrono::Utc;

    fn task(labels: Vec<Label>, body: Option<&str>) -> Task {
        Task {
            author: String::new(),
            id: TaskId {
                source: "github".into(),
                key: "o/r#1".into(),
            },
            title: "t".into(),
            body: body.map(str::to_string),
            state: TaskState::Open,
            role: TaskRole::Author,
            ci: crate::CiStatus::None,
            review: crate::ReviewStatus::None,
            checks: vec![],
            unread_count: 0,
            url: "u".into(),
            repo: Some("o/r".into()),
            branch: None,
            base_branch: None,
            updated_at: Utc::now(),
            created_at: None,
            closed_at: None,
            labels,
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
            parent: None,
            priority: None,
            state_label: None,
            blocked_by: vec![],
            merge_after: vec![],
            contracts: vec![],
            blocked_on: None,
        }
    }

    #[test]
    fn none_when_nothing_declared() {
        assert_eq!(resolve_capability_tier(&task(vec![], None)), None);
        assert_eq!(
            resolve_capability_tier(&task(vec![Label::new("bug")], Some("just some text"))),
            None
        );
    }

    #[test]
    fn label_resolves_each_tier() {
        assert_eq!(
            resolve_capability_tier(&task(vec![Label::new("best")], None)),
            Some(CapabilityTier::Best)
        );
        assert_eq!(
            resolve_capability_tier(&task(vec![Label::new("high")], None)),
            Some(CapabilityTier::High)
        );
        assert_eq!(
            resolve_capability_tier(&task(vec![Label::new("medium")], None)),
            Some(CapabilityTier::Medium)
        );
        assert_eq!(
            resolve_capability_tier(&task(vec![Label::new("low")], None)),
            Some(CapabilityTier::Low)
        );
    }

    #[test]
    fn label_match_is_case_insensitive() {
        assert_eq!(
            resolve_capability_tier(&task(vec![Label::new("HIGH")], None)),
            Some(CapabilityTier::High)
        );
        assert_eq!(
            resolve_capability_tier(&task(vec![Label::new("Low")], None)),
            Some(CapabilityTier::Low)
        );
    }

    #[test]
    fn body_marker_resolves_each_tier() {
        assert_eq!(
            resolve_capability_tier(&task(vec![], Some("give it your @best"))),
            Some(CapabilityTier::Best)
        );
        assert_eq!(
            resolve_capability_tier(&task(vec![], Some("please @high this"))),
            Some(CapabilityTier::High)
        );
        assert_eq!(
            resolve_capability_tier(&task(vec![], Some("@medium priority"))),
            Some(CapabilityTier::Medium)
        );
        assert_eq!(
            resolve_capability_tier(&task(vec![], Some("run it @LOW cost"))),
            Some(CapabilityTier::Low)
        );
    }

    #[test]
    fn body_marker_respects_word_boundaries() {
        // Continuation chars after the word → not a marker.
        assert_eq!(
            resolve_capability_tier(&task(vec![], Some("@highest"))),
            None
        );
        assert_eq!(
            resolve_capability_tier(&task(vec![], Some("@high-1"))),
            None
        );
        assert_eq!(
            resolve_capability_tier(&task(vec![], Some("@high.io"))),
            None
        );
        assert_eq!(
            resolve_capability_tier(&task(vec![], Some("@lowball"))),
            None
        );
        // `@` glued to a preceding identifier → an email-like, not a marker.
        assert_eq!(
            resolve_capability_tier(&task(vec![], Some("me@high"))),
            None
        );
        // Plain word without the `@` sigil → not a marker.
        assert_eq!(
            resolve_capability_tier(&task(vec![], Some("this is high priority"))),
            None
        );
    }

    #[test]
    fn body_marker_accepts_surrounding_punctuation() {
        assert_eq!(
            resolve_capability_tier(&task(vec![], Some("(@high)"))),
            Some(CapabilityTier::High)
        );
        assert_eq!(
            resolve_capability_tier(&task(vec![], Some("priority: @low!"))),
            Some(CapabilityTier::Low)
        );
        assert_eq!(
            resolve_capability_tier(&task(vec![], Some("line one\n@medium\nline three"))),
            Some(CapabilityTier::Medium)
        );
    }

    #[test]
    fn label_wins_over_body_marker() {
        // Label `low`, body says `@high` — the label is authoritative.
        let t = task(vec![Label::new("low")], Some("@high please"));
        assert_eq!(resolve_capability_tier(&t), Some(CapabilityTier::Low));
    }

    #[test]
    fn stronger_tier_wins_when_multiple_labels() {
        let t = task(vec![Label::new("low"), Label::new("high")], None);
        assert_eq!(resolve_capability_tier(&t), Some(CapabilityTier::High));
        let t = task(vec![Label::new("low"), Label::new("medium")], None);
        assert_eq!(resolve_capability_tier(&t), Some(CapabilityTier::Medium));
    }

    #[test]
    fn best_beats_a_co_declared_high() {
        let t = task(vec![Label::new("high"), Label::new("best")], None);
        assert_eq!(resolve_capability_tier(&t), Some(CapabilityTier::Best));
        let t = task(vec![], Some("@high but really @best"));
        assert_eq!(resolve_capability_tier(&t), Some(CapabilityTier::Best));
    }

    #[test]
    fn stronger_tier_wins_when_multiple_body_markers() {
        let t = task(vec![], Some("@low then reconsidered @high"));
        assert_eq!(resolve_capability_tier(&t), Some(CapabilityTier::High));
    }
}
