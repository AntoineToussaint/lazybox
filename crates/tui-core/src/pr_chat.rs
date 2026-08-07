//! "Ask about this PR" support (#945): the context document for the
//! PR-scoped chat that piggybacks on the same headless structured
//! agent run as Ask Lazybox (#302).
//!
//! Ask Lazybox feeds the agent a document describing lazybox itself;
//! this feeds it a document describing one focused PR/issue — its
//! metadata, the activity feed, and (when a worktree is checked out)
//! the local diff — so the same streamed-answer machinery answers
//! questions about the change instead of about the app. The agent
//! selection reuses [`crate::help::select_help_agent`] /
//! [`crate::help::HELP_AGENT_PREFERENCE`].

use lazybox_core::{Activity, ActivityKind, CiStatus, Mergeable, ReviewStatus, Task};
use lazybox_ipc::{DiffLineKindDto, WorkspaceDiffDto};

/// Sentinel session key for the PR-chat agent run. Like
/// [`crate::help::HELP_SESSION_KEY`] it is not a real workspace: the
/// daemon's `resolve_cwd` finds no workspace record for it and resolves
/// a neutral cwd, so the agent answers purely from the context document
/// below (diff-first — it never reads the worktree). Clients recognize
/// the matching `AgentRunStarted` by it.
pub const PR_CHAT_SESSION_KEY: &str = "lazybox:pr-chat";

/// Upper bound on the diff text folded into the context, so a large PR
/// can't blow the prompt. Past it the diff is cut with a note.
const DIFF_CHAR_BUDGET: usize = 60_000;

/// What code context is available for the chat, driving both what the
/// document includes and the honest "what I couldn't see" disclosure
/// the acceptance criteria ask for.
pub enum PrDiff<'a> {
    /// A worktree diff was read. May be empty (clean worktree — the PR's
    /// changes are already committed, so there is nothing uncommitted to
    /// show).
    Available(&'a WorkspaceDiffDto),
    /// A worktree is provisioned but its diff could not be read.
    Unreadable,
    /// No worktree is checked out for this workspace yet.
    NoWorktree,
}

/// Assemble the PR-scoped context document fed as the opening turn of
/// the chat run. Follow-ups ride the same conversation, so this is built
/// once and the diff it captures is the diff the whole thread sees.
pub fn pr_context(task: &Task, activity: &[Activity], diff: PrDiff<'_>) -> String {
    let is_pr = task.is_pr();
    let noun = if is_pr { "pull request" } else { "issue" };
    let mut out = String::with_capacity(16 * 1024);

    out.push_str(&format!(
        "You are lazybox's PR assistant. lazybox is a reactive PR-inbox TUI. The user is \
looking at one {noun} and wants to understand it — what it changes, why, and whether it \
looks right.\n\
\n\
Answer their questions using ONLY the reference below.\n\
\n\
Rules:\n\
- Be concrete and brief. Prefer a few sentences over an essay.\n\
- When your answer is grounded in the diff, cite the file and line (e.g. `src/foo.rs:42`); \
when it is grounded in a comment, name its author.\n\
- The diff below shows the worktree's *uncommitted local changes*, not the full base..head \
PR diff. It is present only when a worktree is checked out. If a question needs code you \
cannot see here, say so plainly rather than guessing.\n\
- Do not use tools, do not read or write files, do not run commands. Everything you have is below.\n",
    ));

    out.push_str("\n# ");
    out.push_str(noun);
    out.push('\n');
    out.push_str(&format!("\nTitle: {}\n", task.title));
    if let Some(repo) = &task.repo {
        out.push_str(&format!("Repo: {repo}\n"));
    }
    if !task.url.is_empty() {
        out.push_str(&format!("URL: {}\n", task.url));
    }
    out.push_str(&format!("Author: {}\n", task.author));
    out.push_str(&format!("State: {}\n", state_label(task)));
    if is_pr {
        if let Some(branch) = &task.branch {
            let base = task.base_branch.as_deref().unwrap_or("the base branch");
            out.push_str(&format!("Branch: {branch} → {base}\n"));
        }
        out.push_str(&format!(
            "CI: {}   Review: {}   Mergeable: {}\n",
            ci_label(task.ci),
            review_label(task.review),
            merge_label(task.mergeable),
        ));
        out.push_str(&format!(
            "Diffstat: +{} -{}\n",
            task.additions, task.deletions
        ));
    }
    if !task.labels.is_empty() {
        let names: Vec<&str> = task.labels.iter().map(|l| l.name.as_str()).collect();
        out.push_str(&format!("Labels: {}\n", names.join(", ")));
    }
    if !task.reviewers.is_empty() {
        out.push_str(&format!("Reviewers: {}\n", task.reviewers.join(", ")));
    }
    if !task.assignees.is_empty() {
        out.push_str(&format!("Assignees: {}\n", task.assignees.join(", ")));
    }

    out.push_str("\n## Description\n\n");
    match task
        .body
        .as_deref()
        .map(str::trim)
        .filter(|b| !b.is_empty())
    {
        Some(body) => {
            out.push_str(body);
            out.push('\n');
        }
        None => out.push_str("_(no description)_\n"),
    }

    push_activity(&mut out, activity);
    push_diff(&mut out, &diff);
    out
}

fn push_activity(out: &mut String, activity: &[Activity]) {
    out.push_str("\n## Activity (newest first)\n\n");
    if activity.is_empty() {
        out.push_str("_(no comments or reviews)_\n");
        return;
    }
    for act in activity {
        let kind = match act.kind {
            ActivityKind::Comment => "comment",
            ActivityKind::Review => "review",
            ActivityKind::StatusChange => "status",
            ActivityKind::CiUpdate => "ci",
        };
        let loc = match (&act.path, act.line) {
            (Some(path), Some(line)) => format!(" on {path}:{line}"),
            (Some(path), None) => format!(" on {path}"),
            _ => String::new(),
        };
        out.push_str(&format!("- {} ({kind}{loc}): ", act.author));
        let body = act.body.trim();
        if body.is_empty() {
            out.push_str("_(empty)_\n");
        } else {
            // Keep comments on their bullet: collapse newlines so one
            // comment stays one entry in the outline.
            out.push_str(&body.replace('\n', " "));
            out.push('\n');
        }
    }
}

fn push_diff(out: &mut String, diff: &PrDiff<'_>) {
    out.push_str("\n## Local diff\n\n");
    let dto = match diff {
        PrDiff::NoWorktree => {
            out.push_str(
                "_No worktree is checked out for this workspace, so no diff is available. \
Answer from the metadata and activity above, and say when a question needs the code._\n",
            );
            return;
        }
        PrDiff::Unreadable => {
            out.push_str(
                "_A worktree is checked out but its diff could not be read. Answer from the \
metadata and activity above, and say when a question needs the code._\n",
            );
            return;
        }
        PrDiff::Available(dto) => dto,
    };

    if dto.files.is_empty() {
        out.push_str(
            "_The worktree has no uncommitted changes — the diff is empty. The PR's changes \
are already committed, so answer about the change from the description and activity, and say \
you cannot see the committed code from here._\n",
        );
        return;
    }

    if !dto.stat.is_empty() {
        out.push_str("```\n");
        for line in &dto.stat {
            out.push_str(line);
            out.push('\n');
        }
        out.push_str("```\n\n");
    }

    out.push_str("```diff\n");
    let mut budget = DIFF_CHAR_BUDGET;
    let mut cut = dto.truncated;
    'files: for file in &dto.files {
        let header = match &file.old_path {
            Some(old) if old != &file.path => format!("--- {old}\n+++ {}\n", file.path),
            _ => format!("+++ {}\n", file.path),
        };
        if !take(&mut budget, header.len()) {
            cut = true;
            break;
        }
        out.push_str(&header);
        for hunk in &file.hunks {
            let hline = format!("{}\n", hunk.header);
            if !take(&mut budget, hline.len()) {
                cut = true;
                break 'files;
            }
            out.push_str(&hline);
            for line in &hunk.lines {
                let prefix = match line.kind {
                    DiffLineKindDto::Addition => '+',
                    DiffLineKindDto::Deletion => '-',
                    DiffLineKindDto::Context => ' ',
                    DiffLineKindDto::Meta => '\\',
                };
                let rendered = format!("{prefix}{}\n", line.text);
                if !take(&mut budget, rendered.len()) {
                    cut = true;
                    break 'files;
                }
                out.push_str(&rendered);
            }
        }
    }
    out.push_str("```\n");
    if cut {
        out.push_str("\n_(diff truncated — ask about a specific file for detail.)_\n");
    }
}

/// Debit `budget` by `cost`, returning `false` (and leaving the budget
/// untouched) when it would overrun.
fn take(budget: &mut usize, cost: usize) -> bool {
    match budget.checked_sub(cost) {
        Some(rest) => {
            *budget = rest;
            true
        }
        None => false,
    }
}

fn state_label(task: &Task) -> &'static str {
    use lazybox_core::TaskState::*;
    match task.state {
        Open => "open",
        InProgress => "in progress",
        InReview => "in review",
        Closed => "closed",
        Merged => "merged",
        Draft => "draft",
    }
}

fn ci_label(ci: CiStatus) -> &'static str {
    match ci {
        CiStatus::None => "none",
        CiStatus::Pending => "pending",
        CiStatus::Running => "running",
        CiStatus::Success => "passing",
        CiStatus::Failure => "failing",
        CiStatus::Mixed => "mixed",
    }
}

fn review_label(review: ReviewStatus) -> &'static str {
    match review {
        ReviewStatus::None => "none",
        ReviewStatus::Pending => "pending",
        ReviewStatus::Approved => "approved",
        ReviewStatus::ChangesRequested => "changes requested",
    }
}

fn merge_label(mergeable: Mergeable) -> &'static str {
    match mergeable {
        Mergeable::Unknown => "unknown",
        Mergeable::Mergeable => "clean",
        Mergeable::Conflicting => "conflicting",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use lazybox_core::{Label, Task, TaskId, TaskKind, TaskRole, TaskState};
    use lazybox_ipc::{DiffFileDto, DiffHunkDto, DiffLineDto};

    fn pr_task() -> Task {
        Task {
            author: "octocat".into(),
            id: TaskId {
                source: "github".into(),
                key: "owner/repo#12".into(),
            },
            title: "Add retry to the poller".into(),
            body: Some("Retries transient poll failures.\n\nCloses #9.".into()),
            state: TaskState::Open,
            role: TaskRole::Reviewer,
            ci: CiStatus::Failure,
            review: ReviewStatus::ChangesRequested,
            checks: vec![],
            unread_count: 0,
            url: "https://github.com/owner/repo/pull/12".into(),
            repo: Some("owner/repo".into()),
            branch: Some("feat/retry".into()),
            base_branch: Some("main".into()),
            updated_at: Utc::now(),
            created_at: None,
            closed_at: None,
            labels: vec![Label::new("bug")],
            reviewers: vec!["reviewer1".into()],
            assignees: vec![],
            auto_merge_enabled: false,
            is_in_merge_queue: false,
            mergeable: Mergeable::Conflicting,
            is_behind_base: false,
            node_id: None,
            needs_reply: false,
            last_commenter: None,
            recent_activity: vec![],
            additions: 20,
            deletions: 3,
            kind: Some(TaskKind::Pr),
            closes_issues: vec![],
            linked_tasks: vec![],
            priority: None,
            state_label: None,
        }
    }

    fn comment(author: &str, body: &str) -> Activity {
        Activity {
            author: author.into(),
            body: body.into(),
            created_at: Utc::now(),
            kind: ActivityKind::Comment,
            node_id: None,
            path: None,
            line: None,
            diff_hunk: None,
            thread_id: None,
        }
    }

    fn sample_diff() -> WorkspaceDiffDto {
        WorkspaceDiffDto {
            status: vec![],
            stat: vec![" src/poll.rs | 4 ++++".into()],
            files: vec![DiffFileDto {
                old_path: None,
                path: "src/poll.rs".into(),
                headers: vec![],
                hunks: vec![DiffHunkDto {
                    header: "@@ -1,2 +1,4 @@".into(),
                    old_start: 1,
                    new_start: 1,
                    lines: vec![
                        DiffLineDto {
                            kind: DiffLineKindDto::Context,
                            text: "fn poll() {".into(),
                            old_line: Some(1),
                            new_line: Some(1),
                        },
                        DiffLineDto {
                            kind: DiffLineKindDto::Addition,
                            text: "    retry(3);".into(),
                            old_line: None,
                            new_line: Some(2),
                        },
                    ],
                }],
            }],
            truncated: false,
        }
    }

    #[test]
    fn context_carries_pr_metadata_and_activity() {
        let task = pr_task();
        let activity = vec![
            comment("reviewer1", "This needs a test."),
            comment("octocat", "Added one."),
        ];
        let ctx = pr_context(&task, &activity, PrDiff::NoWorktree);

        assert!(ctx.contains("Add retry to the poller"));
        assert!(ctx.contains("Author: octocat"));
        assert!(ctx.contains("State: open"));
        assert!(ctx.contains("Branch: feat/retry → main"));
        assert!(ctx.contains("CI: failing"));
        assert!(ctx.contains("Review: changes requested"));
        assert!(ctx.contains("Labels: bug"));
        assert!(ctx.contains("Retries transient poll failures."));
        assert!(ctx.contains("reviewer1 (comment): This needs a test."));
        // No worktree → an explicit disclosure, not a silent omission.
        assert!(ctx.contains("No worktree is checked out"));
    }

    #[test]
    fn context_folds_in_the_diff_when_available() {
        let task = pr_task();
        let diff = sample_diff();
        let ctx = pr_context(&task, &[], PrDiff::Available(&diff));

        assert!(ctx.contains("src/poll.rs | 4 ++++"));
        assert!(ctx.contains("@@ -1,2 +1,4 @@"));
        assert!(ctx.contains("+    retry(3);"));
        assert!(ctx.contains(" fn poll() {"));
    }

    #[test]
    fn empty_worktree_diff_is_disclosed_not_silently_dropped() {
        let task = pr_task();
        let empty = WorkspaceDiffDto {
            status: vec![],
            stat: vec![],
            files: vec![],
            truncated: false,
        };
        let ctx = pr_context(&task, &[], PrDiff::Available(&empty));
        assert!(ctx.contains("no uncommitted changes"));
    }

    #[test]
    fn multiline_comment_stays_on_one_bullet() {
        let task = pr_task();
        let activity = vec![comment("reviewer1", "line one\nline two")];
        let ctx = pr_context(&task, &activity, PrDiff::NoWorktree);
        assert!(ctx.contains("reviewer1 (comment): line one line two"));
    }
}
