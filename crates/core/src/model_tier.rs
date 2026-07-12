//! Priority/effort tier a task declares, and the pure resolver that
//! reads it off a [`Task`].
//!
//! A task can request a right-sized model by declaring a priority:
//! a `high` / `medium` / `low` **label**, or an `@high` / `@medium` /
//! `@low` **marker** in its body. The autonomous ("pilot") spawn paths
//! and the interactive spawn resolve the tier here, then map it to a
//! concrete model via `agent.models` config and inject it at spawn
//! time. High → the strongest model, low → the cheapest/fastest.
//!
//! This module holds only the pure decision (tier ← task); the
//! tier → concrete-model-id mapping lives in `lazybox-config`
//! (`agent.models`), and the injection lives in the agent spawn path.

use crate::Task;
use serde::{Deserialize, Serialize};

/// Priority/effort tier that selects which model an agent spawns with.
///
/// Ordered strongest → cheapest so the resolver can prefer the higher
/// tier when a task somehow declares more than one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ModelTier {
    High,
    Medium,
    Low,
}

impl ModelTier {
    /// Strongest → cheapest. The resolver scans in this order so a task
    /// carrying, say, both a `high` and a `low` label resolves to
    /// `High` (the stronger wins).
    const ORDER: [ModelTier; 3] = [Self::High, Self::Medium, Self::Low];

    /// Lowercase token this tier is declared with — the label name, the
    /// `@`-marker suffix, and the YAML key its concrete model lives
    /// under (`high` / `medium` / `low`).
    pub fn as_str(self) -> &'static str {
        match self {
            Self::High => "high",
            Self::Medium => "medium",
            Self::Low => "low",
        }
    }
}

/// Resolve the [`ModelTier`] a task declares, if any.
///
/// Precedence:
/// 1. A `high` / `medium` / `low` **label** (case-insensitive) wins
///    over a body marker.
/// 2. Otherwise an `@high` / `@medium` / `@low` **marker** in the body,
///    matched at a word boundary (so `@highest` / `email@high` don't
///    count).
///
/// When a source declares more than one tier (two priority labels, or
/// two markers), the stronger tier wins — [`ModelTier::ORDER`] is
/// scanned strongest-first.
///
/// Returns `None` when nothing is declared; the caller falls back to
/// the configured default tier.
pub fn resolve_model_tier(task: &Task) -> Option<ModelTier> {
    tier_from_labels(task).or_else(|| tier_from_body(task.body.as_deref().unwrap_or("")))
}

fn tier_from_labels(task: &Task) -> Option<ModelTier> {
    ModelTier::ORDER.into_iter().find(|tier| {
        task.labels
            .iter()
            .any(|label| label.name.eq_ignore_ascii_case(tier.as_str()))
    })
}

fn tier_from_body(body: &str) -> Option<ModelTier> {
    ModelTier::ORDER
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
            assignees: vec![],
            auto_merge_enabled: false,
            is_in_merge_queue: false,
            mergeable: crate::Mergeable::Mergeable,
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

    #[test]
    fn none_when_nothing_declared() {
        assert_eq!(resolve_model_tier(&task(vec![], None)), None);
        assert_eq!(
            resolve_model_tier(&task(vec![Label::new("bug")], Some("just some text"))),
            None
        );
    }

    #[test]
    fn label_resolves_each_tier() {
        assert_eq!(
            resolve_model_tier(&task(vec![Label::new("high")], None)),
            Some(ModelTier::High)
        );
        assert_eq!(
            resolve_model_tier(&task(vec![Label::new("medium")], None)),
            Some(ModelTier::Medium)
        );
        assert_eq!(
            resolve_model_tier(&task(vec![Label::new("low")], None)),
            Some(ModelTier::Low)
        );
    }

    #[test]
    fn label_match_is_case_insensitive() {
        assert_eq!(
            resolve_model_tier(&task(vec![Label::new("HIGH")], None)),
            Some(ModelTier::High)
        );
        assert_eq!(
            resolve_model_tier(&task(vec![Label::new("Low")], None)),
            Some(ModelTier::Low)
        );
    }

    #[test]
    fn body_marker_resolves_each_tier() {
        assert_eq!(
            resolve_model_tier(&task(vec![], Some("please @high this"))),
            Some(ModelTier::High)
        );
        assert_eq!(
            resolve_model_tier(&task(vec![], Some("@medium priority"))),
            Some(ModelTier::Medium)
        );
        assert_eq!(
            resolve_model_tier(&task(vec![], Some("run it @LOW cost"))),
            Some(ModelTier::Low)
        );
    }

    #[test]
    fn body_marker_respects_word_boundaries() {
        // Continuation chars after the word → not a marker.
        assert_eq!(resolve_model_tier(&task(vec![], Some("@highest"))), None);
        assert_eq!(resolve_model_tier(&task(vec![], Some("@high-1"))), None);
        assert_eq!(resolve_model_tier(&task(vec![], Some("@high.io"))), None);
        assert_eq!(resolve_model_tier(&task(vec![], Some("@lowball"))), None);
        // `@` glued to a preceding identifier → an email-like, not a marker.
        assert_eq!(resolve_model_tier(&task(vec![], Some("me@high"))), None);
        // Plain word without the `@` sigil → not a marker.
        assert_eq!(
            resolve_model_tier(&task(vec![], Some("this is high priority"))),
            None
        );
    }

    #[test]
    fn body_marker_accepts_surrounding_punctuation() {
        assert_eq!(
            resolve_model_tier(&task(vec![], Some("(@high)"))),
            Some(ModelTier::High)
        );
        assert_eq!(
            resolve_model_tier(&task(vec![], Some("priority: @low!"))),
            Some(ModelTier::Low)
        );
        assert_eq!(
            resolve_model_tier(&task(vec![], Some("line one\n@medium\nline three"))),
            Some(ModelTier::Medium)
        );
    }

    #[test]
    fn label_wins_over_body_marker() {
        // Label `low`, body says `@high` — the label is authoritative.
        let t = task(vec![Label::new("low")], Some("@high please"));
        assert_eq!(resolve_model_tier(&t), Some(ModelTier::Low));
    }

    #[test]
    fn stronger_tier_wins_when_multiple_labels() {
        let t = task(vec![Label::new("low"), Label::new("high")], None);
        assert_eq!(resolve_model_tier(&t), Some(ModelTier::High));
        let t = task(vec![Label::new("low"), Label::new("medium")], None);
        assert_eq!(resolve_model_tier(&t), Some(ModelTier::Medium));
    }

    #[test]
    fn stronger_tier_wins_when_multiple_body_markers() {
        let t = task(vec![], Some("@low then reconsidered @high"));
        assert_eq!(resolve_model_tier(&t), Some(ModelTier::High));
    }
}
