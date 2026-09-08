//! Helpers for rendering a task as a single sidebar row:
//!
//! - `pr_number(task)` — extracts the trailing `#NNN` from `task.id.key`.
//!   The pure extractor moved to `lazybox_tui_core::inbox` (#731, it's a
//!   dependency of the client-free search); re-exported here so row
//!   renderers keep their `task_label::pr_number` path.
//! - `task_identifier(task)` / `identifier_width(task)` — the identifier
//!   column's text: the PR number, or a tracker key (`ENG-123`) when the
//!   key has no `#N` suffix.
//! - `identifier_color(task)` — deterministic color for that identifier.
//! - `pr_number_color(n)` — deterministic color from the PR number.
//!   Same number → same color across renders. Stays here — it produces
//!   a ratatui `Color`.

use ratatui::style::Color;

pub use lazybox_tui_core::inbox::{identifier_width, pr_number, task_identifier};

/// Stable color for a task's identifier: the PR number's palette slot
/// for GitHub keys, a hash of the key for tracker identifiers, so
/// `ENG-123` keeps one color across renders like `#312` does.
pub fn identifier_color(task: &lazybox_core::Task) -> Color {
    let n = pr_number(task).unwrap_or_else(|| {
        task.id
            .key
            .bytes()
            .fold(0u64, |h, b| h.wrapping_mul(31).wrapping_add(b as u64))
    });
    pr_number_color(n)
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

/// The issue a PR row was opened from — the `Closes #N` that the
/// issue→PR collapse folded into this workspace (#1528).
///
/// Returns `(identifier, extra)`: the tracker's own identifier for the
/// first originating issue (`298`, `ENG-123`) and how many further
/// issues this PR closes beyond it. `None` for a row that isn't a PR, or
/// a PR that closes nothing.
///
/// The collapse is what makes this worth surfacing: once an issue and
/// its PR share one row, the row shows the PR and the issue it came from
/// simply vanishes from the sidebar — visible only by opening the
/// activity pane. Resolution order matches `RightPane::originating_issue`
/// (folded issue task first, `closes_issues` id as the fallback) so the
/// two surfaces can't name different issues for the same row.
pub fn originating_issue(workspace: &lazybox_core::Workspace) -> Option<(String, usize)> {
    let pr = workspace.pr.as_ref()?;
    let mut folded = workspace
        .gh_issues
        .iter()
        .chain(workspace.linear_issues.iter());
    let extra_folded = folded.clone().count().saturating_sub(1);
    if let Some(issue) = folded.next() {
        let id = lazybox_tui_core::inbox::task_identifier(issue)?;
        return Some((id, extra_folded));
    }
    // No folded task yet (the issue hasn't been polled into this
    // workspace) — fall back to the PR's own `Closes #N` reference.
    let first = pr.closes_issues.first()?;
    let (_, number) = first.key.rsplit_once('#')?;
    Some((number.to_string(), pr.closes_issues.len().saturating_sub(1)))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Minimal task; `Task` has no `Default`, and only the id and the
    /// `closes_issues` link matter here.
    fn task(source: &str, key: &str) -> lazybox_core::Task {
        lazybox_core::Task {
            author: String::new(),
            id: lazybox_core::TaskId {
                source: source.into(),
                key: key.into(),
            },
            title: "t".into(),
            body: None,
            state: lazybox_core::TaskState::Open,
            role: lazybox_core::TaskRole::Author,
            ci: lazybox_core::CiStatus::None,
            review: lazybox_core::ReviewStatus::None,
            checks: vec![],
            unread_count: 0,
            url: String::new(),
            repo: Some("owner/repo".into()),
            branch: None,
            base_branch: None,
            updated_at: chrono::Utc::now(),
            created_at: None,
            closed_at: None,
            labels: vec![],
            reviewers: vec![],
            reviews: vec![],
            assignees: vec![],
            auto_merge_enabled: false,
            is_in_merge_queue: false,
            mergeable: lazybox_core::Mergeable::Unknown,
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
            blocked_by: vec![],
            merge_after: vec![],
            blocked_on: None,
            priority: None,
            state_label: None,
        }
    }

    /// #1528: a PR row must name the issue it closes. The folded issue
    /// task wins when polling has attached it; the PR's own
    /// `closes_issues` reference is the fallback before that happens, so
    /// the chip appears immediately rather than one poll later.
    #[test]
    fn originating_issue_prefers_the_folded_task_then_falls_back() {
        // A `/pull/` URL is what classifies the task as a PR, so the
        // workspace files it under `pr` rather than `gh_issues`.
        let mut pr = task(lazybox_core::GITHUB_SOURCE, "owner/repo#312");
        pr.url = "https://github.com/owner/repo/pull/312".into();
        let mut ws = lazybox_core::Workspace::from_task(pr, chrono::Utc::now());
        assert!(ws.pr.is_some(), "fixture must be a PR workspace");

        // Nothing linked yet.
        assert_eq!(originating_issue(&ws), None);

        // Fallback: the PR says `Closes #298` but the issue task hasn't
        // been folded in yet.
        if let Some(pr) = ws.pr.as_mut() {
            pr.closes_issues = vec![lazybox_core::TaskId {
                source: lazybox_core::GITHUB_SOURCE.into(),
                key: "owner/repo#298".into(),
            }];
        }
        assert_eq!(originating_issue(&ws), Some(("298".to_string(), 0)));

        // Once folded, the attached task wins — and a Linear ticket
        // carries its tracker key rather than a bare number.
        ws.linear_issues = vec![task(lazybox_core::LINEAR_SOURCE, "ENG-12")];
        assert_eq!(originating_issue(&ws), Some(("ENG-12".to_string(), 0)));
    }

    /// A row that isn't a PR has no "issue it came from" — the chip is
    /// for the collapse, not for every row with a link.
    #[test]
    fn originating_issue_is_none_without_a_pr() {
        let ws = lazybox_core::Workspace::from_task(
            task(lazybox_core::GITHUB_SOURCE, "owner/repo#298"),
            chrono::Utc::now(),
        );
        assert_eq!(originating_issue(&ws), None);
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
