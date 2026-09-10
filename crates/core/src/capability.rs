//! Model-**capability** tier a task declares, and the pure resolver that
//! reads it off a [`Task`].
//!
//! A task names the model two ways. The explicit spelling is a
//! **`model:<token>` label** (or an `@model:<token>` body marker) —
//! `model:l` names a tier alias, `model:opus` a tier label,
//! `model:claude-opus-5` the id a tier pins (#1600). The older spelling
//! is a capability tier: a `best` / `high` / `medium` / `low` **label**,
//! or an `@best` / `@high` / `@medium` / `@low` **marker** in its body,
//! routed through the agent's `models.capability` map. The autonomous
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

    /// Every tier, strongest first — for callers that walk the whole
    /// set rather than resolve one off a task.
    pub const ALL: [CapabilityTier; 4] = Self::ORDER;

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

/// Namespace prefix of an explicit model label (`model:opus`) and of
/// the body marker that spells the same thing (`@model:opus`).
const MODEL_PREFIX: &str = "model:";

/// Which of a task's fields a resolve may read.
///
/// Attaching a **label** needs write access to the repository;
/// **anyone** can open an issue and write its body. So a spawn whose
/// trigger came from a foreign actor reads labels only — otherwise a
/// drive-by issue body could pick the most expensive tier on the
/// autonomous path, the one path with no human at the keyboard (#1600).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeclarationScope {
    /// Labels and body.
    All,
    /// Labels only — the body is untrusted for this spawn.
    LabelsOnly,
}

/// What a task declares about the model it wants.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ModelRequest {
    /// A `model:<token>` label or `@model:<token>` marker. The token
    /// names a tier of the target agent's own menu — see
    /// [`AgentModels::tier_for_token`](crate::AgentModels::tier_for_token).
    Tier(String),
    /// A capability word, routed through the agent's `capability` map.
    Capability(CapabilityTier),
}

impl ModelRequest {
    /// The token as declared, for logs and notices.
    pub fn token(&self) -> &str {
        match self {
            Self::Tier(token) => token,
            Self::Capability(tier) => tier.as_str(),
        }
    }

    /// True for the capability-word spelling, which names urgency but
    /// selects a model — reported at spawn so the rename is
    /// discoverable (#1600).
    pub fn is_capability_word(&self) -> bool {
        matches!(self, Self::Capability(_))
    }
}

/// Every model declaration on `task`, grouped into **precedence ranks**
/// and ordered highest rank first:
///
/// 1. `model:<token>` **labels**.
/// 2. The strongest capability **label**.
/// 3. `@model:<token>` **body markers**.
/// 4. The strongest capability **marker**, matched at a word boundary
///    (so `@highest` / `email@high` miss).
///
/// A label outranks a body marker, and within each source the explicit
/// spelling outranks the capability word — a repo mid-migration carries
/// both without the old word winning.
///
/// Ranks rather than a single winner because a declaration is only
/// meaningful against an agent's menu, which this crate cannot see:
/// picking one token here and handing back a dead end would let an
/// unrelated `model:*` label (a repo that versions its own ML models,
/// say, or a plain typo) *suppress* a `high` label that would have
/// resolved, silently dropping the task to the agent's default. The
/// menu walks the ranks and takes the first that resolves — see
/// [`AgentModels::choose_model`](crate::AgentModels::choose_model).
///
/// Members of a rank are equally authoritative, so their order carries
/// no meaning: the provider does not promise a stable label order, and
/// `choose_model` refuses a rank whose members name different tiers
/// rather than let that order decide which model runs.
///
/// Empty when the task declares nothing; the caller then falls back to
/// the agent's configured default tier.
pub fn resolve_model_requests(task: &Task, scope: DeclarationScope) -> Vec<Vec<ModelRequest>> {
    let body = match scope {
        DeclarationScope::All => task.body.as_deref().unwrap_or(""),
        DeclarationScope::LabelsOnly => "",
    };
    [
        model_from_labels(task),
        capability_from_labels(task)
            .map(ModelRequest::Capability)
            .into_iter()
            .collect(),
        model_from_body(body),
        capability_from_body(body)
            .map(ModelRequest::Capability)
            .into_iter()
            .collect(),
    ]
    .into_iter()
    .filter(|rank: &Vec<ModelRequest>| !rank.is_empty())
    .collect()
}

/// The capability tier a task declares, if any — the `best` / `high` /
/// `medium` / `low` word on its own, ignoring the explicit `model:`
/// spelling that outranks it.
///
/// [`resolve_model_requests`] is what the spawn path uses; this stays
/// as the narrow question "which capability word is on this task?",
/// which is what the boundary tests below pin down.
pub fn resolve_capability_tier(task: &Task) -> Option<CapabilityTier> {
    capability_from_labels(task)
        .or_else(|| capability_from_body(task.body.as_deref().unwrap_or("")))
}

/// Every `model:<token>` label, in the order the provider reported them
/// — one precedence rank, so the order is not allowed to matter.
fn model_from_labels(task: &Task) -> Vec<ModelRequest> {
    task.labels
        .iter()
        .filter_map(|label| {
            let name = label.name.trim();
            let prefix = name.get(..MODEL_PREFIX.len())?;
            if !prefix.eq_ignore_ascii_case(MODEL_PREFIX) {
                return None;
            }
            let token = name[MODEL_PREFIX.len()..].trim();
            (!token.is_empty()).then(|| ModelRequest::Tier(token.to_string()))
        })
        .collect()
}

/// Every `@model:<token>` marker in `body`, with the same pre-boundary
/// rule as [`contains_at_marker`] (the `@` must not be glued to a
/// preceding login char, so `me@model:l` is an address, not a marker).
///
/// The token runs to the first char that can't appear in a tier alias,
/// label word or model id — ids carry `-` and `.`
/// (`claude-haiku-4-5`), so a trailing `.` is shed as sentence
/// punctuation rather than read as part of the id.
fn model_from_body(body: &str) -> Vec<ModelRequest> {
    let mut found = Vec::new();
    let bytes = body.as_bytes();
    let needle = format!("@{MODEL_PREFIX}");
    let n = needle.len();
    if bytes.len() < n {
        return found;
    }
    for i in 0..=bytes.len() - n {
        if !bytes[i..i + n]
            .iter()
            .zip(needle.as_bytes())
            .all(|(a, b)| a.eq_ignore_ascii_case(b))
        {
            continue;
        }
        if i > 0 {
            let prev = bytes[i - 1];
            if is_login_char(prev) || prev == b'@' {
                continue;
            }
        }
        let token: String = body[i + n..]
            .chars()
            .take_while(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
            .collect();
        let token = token.trim_end_matches('.');
        if !token.is_empty() {
            found.push(ModelRequest::Tier(token.to_string()));
        }
    }
    found
}

fn capability_from_labels(task: &Task) -> Option<CapabilityTier> {
    CapabilityTier::ORDER.into_iter().find(|tier| {
        task.labels
            .iter()
            .any(|label| label.name.eq_ignore_ascii_case(tier.as_str()))
    })
}

fn capability_from_body(body: &str) -> Option<CapabilityTier> {
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
