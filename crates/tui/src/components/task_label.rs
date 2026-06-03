//! Helpers for rendering a task as a single sidebar row:
//!
//! - `pr_number(task)` — extracts the trailing `#NNN` from `task.id.key`
//!   (works for both PRs and issues; GitHub uses one number space).
//! - `pr_number_color(n)` — deterministic color from the PR number.
//!   Same number → same color across renders.

use lazybox_core::Task;
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

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use lazybox_core::{CiStatus, ReviewStatus, Task, TaskId, TaskRole, TaskState};

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
            closed_at: None,
            labels: vec![],
            reviewers: vec![],
            assignees: vec![],
            auto_merge_enabled: false,
            is_in_merge_queue: false,
            mergeable: lazybox_core::Mergeable::Mergeable,
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
}
