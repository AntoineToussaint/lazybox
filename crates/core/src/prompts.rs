//! Agent prompt builders that depend only on `Task` data.
//!
//! Lives in `pilot-core` (not `pilot-tui-core`) so the server can
//! build the same prompts for non-UI-triggered spawns — today the
//! auto-spawn flow that fires on a `@pilot` GitHub mention. Keeping
//! one canonical implementation prevents the UI and the auto-spawn
//! path from drifting.

use crate::Task;

/// Prompt for "implement this GitHub issue". Same string the
/// sidebar's `w` shortcut builds when the focused workspace is an
/// issue, so a press-`w` spawn and a `@pilot`-mention auto-spawn
/// land the agent in identical context.
pub fn build_implement_issue_prompt(issue: &Task) -> String {
    let issue_number = issue
        .id
        .key
        .rsplit_once('#')
        .map_or(issue.id.key.as_str(), |(_, n)| n);
    let repo = issue.repo.as_deref().unwrap_or("the repository");
    let body_block = match issue
        .body
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        Some(body) => format!("\n\nIssue body:\n{body}\n"),
        None => String::new(),
    };
    format!(
        "Implement GitHub issue #{issue_number} in {repo}: {title}.\
         {body_block}\
         \nWalk through it: create a fresh branch from the repo's default base, \
         implement the change end-to-end (code + tests), run the project's local \
         checks until they pass, then open the PR with `gh pr create`. \
         Reference the issue both ways so it links cleanly and collapses to a \
         single row in pilot:\
         \n- Title: derive a concise PR title (don't copy the issue title \
         verbatim) and append the issue number as a trailing suffix, e.g. \
         `Add foo to bar (#{issue_number})`.\
         \n- Body: put a closing keyword on its own line so GitHub auto-closes \
         the issue on merge — start the body with `Closes #{issue_number}.` \
         followed by a `## Summary` section. If the PR addresses more than one \
         issue, give each its own `Closes #N.` line; for an issue in a different \
         repo use the full `Closes owner/repo#N.` form.\
         \nReply with the PR URL when it's open.",
        title = issue.title,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{CiStatus, ReviewStatus, Task, TaskId, TaskRole, TaskState};
    use chrono::Utc;

    fn issue(repo: &str, n: u64, title: &str, body: Option<&str>) -> Task {
        Task {
            id: TaskId {
                source: "github".into(),
                key: format!("{repo}#{n}"),
            },
            title: title.into(),
            body: body.map(str::to_string),
            state: TaskState::Open,
            role: TaskRole::Mentioned,
            ci: CiStatus::None,
            review: ReviewStatus::None,
            checks: vec![],
            unread_count: 0,
            url: format!("https://github.com/{repo}/issues/{n}"),
            repo: Some(repo.into()),
            branch: None,
            base_branch: None,
            updated_at: Utc::now(),
            labels: vec![],
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
    fn includes_issue_number_repo_title() {
        let prompt = build_implement_issue_prompt(&issue("acme/widget", 42, "Add dark mode", None));
        assert!(prompt.contains("#42"));
        assert!(prompt.contains("acme/widget"));
        assert!(prompt.contains("Add dark mode"));
        assert!(prompt.contains("Closes #42"));
    }

    #[test]
    fn instructs_title_suffix_and_closing_body() {
        let prompt = build_implement_issue_prompt(&issue("acme/widget", 42, "Add dark mode", None));
        // Title guidance: trailing `(#N)` suffix, derived (not verbatim) title.
        assert!(prompt.contains("(#42)"));
        assert!(prompt.contains("trailing suffix"));
        assert!(prompt.contains("don't copy the issue title"));
        // Body guidance: closing keyword on its own line for auto-close.
        assert!(prompt.contains("Closes #42."));
        assert!(prompt.contains("auto-close"));
    }

    #[test]
    fn covers_multi_issue_and_cross_repo_forms() {
        let prompt = build_implement_issue_prompt(&issue("acme/widget", 42, "Add dark mode", None));
        // Multiple linked issues each get their own line.
        assert!(prompt.contains("its own `Closes #N.` line"));
        // Cross-repo issues use the full owner/repo#N form.
        assert!(prompt.contains("owner/repo#N"));
    }

    #[test]
    fn body_block_only_when_non_empty() {
        let with_body =
            build_implement_issue_prompt(&issue("o/r", 7, "X", Some("Steps to repro: …")));
        assert!(with_body.contains("Issue body:"));
        let empty = build_implement_issue_prompt(&issue("o/r", 7, "X", Some("   ")));
        assert!(!empty.contains("Issue body:"));
        let none = build_implement_issue_prompt(&issue("o/r", 7, "X", None));
        assert!(!none.contains("Issue body:"));
    }
}
