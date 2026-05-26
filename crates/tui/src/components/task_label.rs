//! Helpers for rendering a task as a single sidebar row:
//!
//! - `pr_number(task)` — extracts the trailing `#NNN` from `task.id.key`
//!   (works for both PRs and issues; GitHub uses one number space).
//! - `parse_conventional_prefix(title)` — splits a conventional-commit
//!   prefix off the front of a title (`feat:`, `feat(scope):`, `[feat]`,
//!   `[FEAT]`, with optional `!` for breaking). Returns the kind and
//!   the remaining title.
//! - `pr_number_color(n)` — deterministic color from the PR number.
//!   Same number → same color across renders.
//! - `kind_color(k)` — fixed color per conventional-commit kind, picked
//!   to match common conventions (feat=green, fix=red, …).
//!
//! Kept in its own module because the parsing has edge cases worth
//! testing in isolation (the sidebar render path does too much else
//! to be a good unit-test target).

use pilot_core::Task;
use ratatui::style::Color;

/// Convert `Task.id.key` (e.g. `"owner/repo#1234"`) to the trailing
/// integer. Returns `None` when there's no `#`-suffix or the suffix
/// isn't a number — those cases shouldn't happen for GitHub-derived
/// tasks today, but we don't want to panic if Linear or a custom
/// provider lands here.
pub fn pr_number(task: &Task) -> Option<u64> {
    let key = task.id.key.as_str();
    let (_, num) = key.rsplit_once('#')?;
    num.parse().ok()
}

/// Conventional-commit kinds we recognise. Anything else falls back to
/// "no prefix" — we don't try to invent a color for unknown words
/// because they're more likely to be normal title nouns ("Add" /
/// "Update") than a typo'd commit type.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConventionalKind {
    Feat,
    Fix,
    Chore,
    Refactor,
    Docs,
    Test,
    Perf,
    Build,
    Ci,
    Style,
    Revert,
}

impl ConventionalKind {
    /// Short uppercase label rendered in the sidebar. Kept tight
    /// (3-4 chars where possible) so the badge doesn't eat the title.
    pub fn label(self) -> &'static str {
        match self {
            Self::Feat => "FEAT",
            Self::Fix => "FIX",
            Self::Chore => "CHORE",
            Self::Refactor => "RFCTR",
            Self::Docs => "DOCS",
            Self::Test => "TEST",
            Self::Perf => "PERF",
            Self::Build => "BUILD",
            Self::Ci => "CI",
            Self::Style => "STYLE",
            Self::Revert => "REVRT",
        }
    }

    fn from_word(s: &str) -> Option<Self> {
        // Strip a trailing `!` (breaking-change marker) before the
        // table lookup so `feat!:` and `[feat!]` both classify.
        let s = s.strip_suffix('!').unwrap_or(s);
        match s.to_ascii_lowercase().as_str() {
            "feat" | "feature" => Some(Self::Feat),
            "fix" | "bugfix" => Some(Self::Fix),
            "chore" => Some(Self::Chore),
            "refactor" | "refac" => Some(Self::Refactor),
            "docs" | "doc" => Some(Self::Docs),
            "test" | "tests" => Some(Self::Test),
            "perf" => Some(Self::Perf),
            "build" => Some(Self::Build),
            "ci" => Some(Self::Ci),
            "style" => Some(Self::Style),
            "revert" => Some(Self::Revert),
            _ => None,
        }
    }
}

/// Try to peel a conventional-commit prefix off the front of `title`.
/// Returns `(kind, remaining_title)` on success, where the remaining
/// title has any leading whitespace + separator stripped.
///
/// Recognised forms (case-insensitive):
/// - `feat: rest…` and `feat!: rest…`
/// - `feat(scope): rest…` and `feat(scope)!: rest…`
/// - `[feat] rest…` and `[FEAT!] rest…`
///
/// "Rest" can have leading punctuation/space.
pub fn parse_conventional_prefix(title: &str) -> Option<(ConventionalKind, &str)> {
    let trimmed = title.trim_start();
    if let Some((kind, rest)) = parse_bracket_form(trimmed) {
        return Some((kind, rest.trim_start()));
    }
    if let Some((kind, rest)) = parse_colon_form(trimmed) {
        return Some((kind, rest.trim_start()));
    }
    None
}

/// `[feat] rest…`, `[FEAT] rest…`, `[feat!] rest…`. We don't try to
/// support a scope inside brackets — that's not a convention people
/// actually use.
fn parse_bracket_form(s: &str) -> Option<(ConventionalKind, &str)> {
    let rest = s.strip_prefix('[')?;
    let close = rest.find(']')?;
    let word = &rest[..close];
    let kind = ConventionalKind::from_word(word)?;
    Some((kind, &rest[close + 1..]))
}

/// `feat: rest…`, `feat(scope): rest…`, `feat!: rest…`. We split on
/// the first `:` and validate the word; the optional `(scope)` chunk
/// gets dropped from the parsed kind (we don't render it today).
fn parse_colon_form(s: &str) -> Option<(ConventionalKind, &str)> {
    let colon = s.find(':')?;
    let head = &s[..colon];
    let rest = &s[colon + 1..];
    // Strip optional `(scope)` and a trailing `!` from the head.
    let head_no_scope = if let Some(paren) = head.find('(') {
        &head[..paren]
    } else {
        // Tolerate a trailing `!` directly on the kind word.
        head.strip_suffix('!').unwrap_or(head)
    };
    let kind = ConventionalKind::from_word(head_no_scope.trim())?;
    Some((kind, rest))
}

/// Stable color for a PR number. Same number → same color across
/// renders (and across launches — no RNG state). Picked from a
/// 6-color palette that stays readable on dark terminal backgrounds.
pub fn pr_number_color(n: u64) -> Color {
    // Deliberately small palette: the goal is "different from your
    // neighbour", not "256 unique colors". Adjacent PR numbers tend
    // to fall in different slots which is what the eye notices.
    const PALETTE: [Color; 6] = [
        Color::Cyan,
        Color::Magenta,
        Color::Blue,
        Color::Yellow,
        Color::Green,
        Color::LightRed,
    ];
    PALETTE[(n as usize) % PALETTE.len()]
}

/// Color for a conventional-commit kind. Tracks common conventions
/// (feat=green, fix=red) so the eye learns the mapping fast.
pub fn kind_color(k: ConventionalKind) -> Color {
    match k {
        ConventionalKind::Feat => Color::Green,
        ConventionalKind::Fix => Color::Red,
        ConventionalKind::Chore => Color::DarkGray,
        ConventionalKind::Refactor => Color::Blue,
        ConventionalKind::Docs => Color::Cyan,
        ConventionalKind::Test => Color::Magenta,
        ConventionalKind::Perf => Color::Yellow,
        ConventionalKind::Build => Color::Yellow,
        ConventionalKind::Ci => Color::Magenta,
        ConventionalKind::Style => Color::DarkGray,
        ConventionalKind::Revert => Color::Red,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use pilot_core::{CiStatus, ReviewStatus, Task, TaskId, TaskRole, TaskState};

    fn task_with_key(key: &str) -> Task {
        Task {
            id: TaskId {
                source: "github".into(),
                key: key.into(),
            },
            title: String::new(),
            body: None,
            state: TaskState::Open,
            role: TaskRole::Author,
            ci: CiStatus::None,
            review: ReviewStatus::None,
            checks: vec![],
            unread_count: 0,
            url: String::new(),
            repo: None,
            branch: None,
            base_branch: None,
            updated_at: Utc::now(),
            labels: vec![],
            reviewers: vec![],
            assignees: vec![],
            auto_merge_enabled: false,
            is_in_merge_queue: false,
            mergeable: pilot_core::Mergeable::Mergeable,
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
    fn pr_number_extracts_trailing_int() {
        assert_eq!(pr_number(&task_with_key("owner/repo#1234")), Some(1234));
        assert_eq!(pr_number(&task_with_key("o/r#1")), Some(1));
    }

    #[test]
    fn pr_number_returns_none_when_no_hash() {
        assert_eq!(pr_number(&task_with_key("plain-key")), None);
    }

    #[test]
    fn pr_number_returns_none_for_non_numeric_suffix() {
        assert_eq!(pr_number(&task_with_key("o/r#abc")), None);
    }

    #[test]
    fn parses_simple_colon_form() {
        let (k, rest) = parse_conventional_prefix("feat: add login").unwrap();
        assert_eq!(k, ConventionalKind::Feat);
        assert_eq!(rest, "add login");
    }

    #[test]
    fn parses_colon_form_with_scope() {
        let (k, rest) = parse_conventional_prefix("fix(auth): handle expired tokens").unwrap();
        assert_eq!(k, ConventionalKind::Fix);
        assert_eq!(rest, "handle expired tokens");
    }

    #[test]
    fn parses_breaking_change_marker() {
        let (k, rest) = parse_conventional_prefix("feat!: rewrite api").unwrap();
        assert_eq!(k, ConventionalKind::Feat);
        assert_eq!(rest, "rewrite api");
    }

    #[test]
    fn parses_bracket_form_uppercase() {
        let (k, rest) = parse_conventional_prefix("[FEAT] add x").unwrap();
        assert_eq!(k, ConventionalKind::Feat);
        assert_eq!(rest, "add x");
    }

    #[test]
    fn parses_bracket_form_with_breaking() {
        let (k, rest) = parse_conventional_prefix("[feat!] add x").unwrap();
        assert_eq!(k, ConventionalKind::Feat);
        assert_eq!(rest, "add x");
    }

    #[test]
    fn returns_none_for_unknown_word() {
        assert!(parse_conventional_prefix("wip: random text").is_none());
        assert!(parse_conventional_prefix("[wip] random text").is_none());
    }

    #[test]
    fn returns_none_for_no_prefix() {
        assert!(parse_conventional_prefix("Add config snapshots metadata").is_none());
    }

    #[test]
    fn handles_leading_whitespace() {
        let (k, _) = parse_conventional_prefix("   feat: x").unwrap();
        assert_eq!(k, ConventionalKind::Feat);
    }

    #[test]
    fn pr_number_color_is_deterministic() {
        // Same number gives same color across calls.
        assert_eq!(pr_number_color(42), pr_number_color(42));
        assert_eq!(pr_number_color(0), pr_number_color(0));
    }

    #[test]
    fn pr_number_color_varies_across_palette() {
        // Six distinct PR numbers should hit every palette slot.
        let colors: std::collections::HashSet<_> = (0..6).map(pr_number_color).collect();
        assert_eq!(colors.len(), 6);
    }

    #[test]
    fn aliases_collapse_to_canonical_kind() {
        assert_eq!(
            parse_conventional_prefix("feature: x").unwrap().0,
            ConventionalKind::Feat
        );
        assert_eq!(
            parse_conventional_prefix("bugfix: x").unwrap().0,
            ConventionalKind::Fix
        );
        assert_eq!(
            parse_conventional_prefix("doc: x").unwrap().0,
            ConventionalKind::Docs
        );
    }
}
