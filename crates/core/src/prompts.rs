//! Agent prompt builders that depend only on `Task` data.
//!
//! Lives in `pilot-core` (not `pilot-tui-core`) so the server can
//! build the same prompts for non-UI-triggered spawns — today the
//! auto-spawn flow that fires on a `@pilot` GitHub mention. Keeping
//! one canonical implementation prevents the UI and the auto-spawn
//! path from drifting.

use crate::Task;

/// The role/preamble + engineering principles every agent run is
/// prefixed with. Versioned as a reviewable artifact at the repo
/// root (`prompts/agent-work.md`) rather than inlined here so the
/// principles can evolve through PRs, auditable on their own.
pub const AGENT_WORK_PREAMBLE: &str = include_str!("../../../prompts/agent-work.md");

/// The trailing `#N` of a GitHub `TaskId::key` (`"owner/repo#42"` →
/// `"42"`), or the whole key when it has no `#`. Centralizes the
/// rsplit-and-fallback every prompt builder needs to name the
/// issue/PR. (The server's `task_number_from_key` parses the same
/// suffix to a `u64`; this returns the `&str` for interpolation.)
fn task_number_str(task: &Task) -> &str {
    task.id
        .key
        .rsplit_once('#')
        .map_or(task.id.key.as_str(), |(_, n)| n)
}

/// Prompt for "implement this GitHub issue". Same string the
/// sidebar's `w` shortcut builds when the focused workspace is an
/// issue, so a press-`w` spawn and a `@pilot`-mention auto-spawn
/// land the agent in identical context.
///
/// Structure: role/preamble + principles ([`AGENT_WORK_PREAMBLE`]),
/// then the concrete task last so the issue is the freshest thing in
/// context.
pub fn build_implement_issue_prompt(issue: &Task) -> String {
    let issue_number = task_number_str(issue);
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
        "{preamble}\n\n---\n\n\
         ## Task\n\n\
         Implement GitHub issue #{issue_number} in {repo}: {title}.\
         {body_block}\
         \nTitle the PR `… (#{issue_number})` and start its body with \
         `Closes #{issue_number}.` so the issue and PR collapse to a single \
         row in pilot.",
        preamble = AGENT_WORK_PREAMBLE.trim_end(),
        title = issue.title,
    )
}

/// Bulleted list of the PR's failing checks (`- name — url`), or a
/// single fallback line when GitHub gave us the rolled-up `Failure`
/// but no per-check breakdown. Shared by the CI-fix prompt; pulled out
/// so the formatting is testable on its own.
fn failing_checks_block(task: &Task) -> String {
    let mut lines: Vec<String> = task
        .checks
        .iter()
        .filter(|c| c.status == crate::CiStatus::Failure)
        .map(|c| match &c.url {
            Some(url) => format!("- {} — {url}", c.name),
            None => format!("- {}", c.name),
        })
        .collect();
    if lines.is_empty() {
        lines.push("- (GitHub reported a failing check but no per-check detail)".into());
    }
    lines.join("\n")
}

/// Prompt for "this PR's CI is red — root-cause and push a fix." Built
/// from `Task` data so the auto-inject flow and any future
/// UI-triggered "fix CI" shortcut share one string. Follows the same
/// preamble-then-task shape as [`build_implement_issue_prompt`] (the
/// #59 principles apply equally), with two task-specific overrides: the
/// agent pulls the failing logs itself (we don't ship a stale tail in
/// the prompt — `gh` in the worktree has the freshest output), and it
/// pushes to the EXISTING PR branch instead of opening a new PR.
pub fn build_fix_ci_prompt(task: &Task) -> String {
    let pr_number = task_number_str(task);
    let repo = task.repo.as_deref().unwrap_or("the repository");
    let branch = task.branch.as_deref().unwrap_or("the PR branch");
    let checks = failing_checks_block(task);
    format!(
        "{preamble}\n\n---\n\n\
         ## Task\n\n\
         CI is failing on PR #{pr_number} in {repo} ({title}) — root-cause it and \
         push a fix to the SAME PR branch (`{branch}`, already checked out here). \
         This is an existing PR: do NOT open a new one; pushing re-runs the checks.\
         \n\nFailing checks:\n{checks}\
         \n\nPull the failing logs before you theorize — `gh pr checks {pr_number}` \
         for the summary, then `gh run view --log-failed` (or open the check URLs \
         above) to read the tail of the failing step. Reproduce locally where you \
         can.\
         \n\nReply with a one-line root-cause + fix summary once the push is up.",
        preamble = AGENT_WORK_PREAMBLE.trim_end(),
        title = task.title,
    )
}

/// Prompt for "this PR conflicts with its base — rebase/merge and
/// resolve." Built from `Task` data, same preamble-then-task shape as
/// the others. We don't have the conflicted-file list up front (GitHub
/// doesn't surface it without attempting the merge), so the agent is
/// told exactly how to enumerate it itself after starting the merge,
/// and to push to the EXISTING PR branch.
pub fn build_fix_conflict_prompt(task: &Task) -> String {
    let pr_number = task_number_str(task);
    let repo = task.repo.as_deref().unwrap_or("the repository");
    let branch = task.branch.as_deref().unwrap_or("the PR branch");
    let base = task.base_branch.as_deref().unwrap_or("the base branch");
    format!(
        "{preamble}\n\n---\n\n\
         ## Task\n\n\
         PR #{pr_number} in {repo} ({title}) has merge conflicts with its base \
         (`{base}`). Bring the branch up to date, resolve the conflicts, and push \
         to the SAME PR branch (`{branch}`, already checked out here). This is an \
         existing PR: do NOT open a new one.\
         \n\nSteps:\
         \n- `git fetch origin {base}`, then `git merge origin/{base}` — prefer \
         merge over rebase so you don't rewrite the PR's existing commits beyond \
         what the conflict requires.\
         \n- List the conflicts with `git diff --name-only --diff-filter=U` and \
         resolve each, preserving BOTH the PR's intent and the incoming base \
         changes — never blindly take one side.\
         \n- Build and run the project's checks to confirm the resolution holds, \
         then commit the merge and push.\
         \n\nReply with the files you resolved and a one-line note on any \
         non-trivial resolution once the push is up.",
        preamble = AGENT_WORK_PREAMBLE.trim_end(),
        title = task.title,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{CheckRun, CiStatus, ReviewStatus, Task, TaskId, TaskRole, TaskState};
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
            closed_at: None,
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
        assert!(prompt.contains("Don't copy the issue title verbatim"));
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
    fn embeds_principles_preamble_before_the_task() {
        let prompt = build_implement_issue_prompt(&issue("acme/widget", 42, "Add dark mode", None));
        // The versioned principles file is prepended verbatim.
        assert!(prompt.contains(AGENT_WORK_PREAMBLE.trim_end()));
        // A representative principle from each major section is present.
        assert!(prompt.contains("Do only what the work item asks"));
        assert!(prompt.contains("Find the root cause before you patch"));
        assert!(prompt.contains("Default to no comments"));
        // Structure: principles come first, the concrete task last.
        let task_at = prompt.find("## Task").expect("task section present");
        let scope_at = prompt.find("## Scope").expect("scope section present");
        assert!(scope_at < task_at, "principles must precede the task");
    }

    #[test]
    fn preamble_keeps_every_principle_section() {
        // The markdown is the agent's behavioral spec; an accidental
        // edit that drops a section would silently weaken every run.
        // Guard each section header so that regression fails CI.
        for header in [
            "## Scope",
            "## Correctness over breadth",
            "## Defensive coding hygiene",
            "## Comments and docs",
            "## Tests",
            "## PR hygiene",
            "## Reversibility and safety",
            "## Workflow",
        ] {
            assert!(
                AGENT_WORK_PREAMBLE.contains(header),
                "missing principle section: {header}"
            );
        }
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

    fn pr(repo: &str, n: u64, title: &str) -> Task {
        let mut t = issue(repo, n, title, None);
        t.id.key = format!("{repo}#{n}");
        t.url = format!("https://github.com/{repo}/pull/{n}");
        t.role = TaskRole::Author;
        t.branch = Some("feature/x".into());
        t.base_branch = Some("main".into());
        t
    }

    #[test]
    fn fix_ci_prompt_includes_pr_repo_branch_and_failing_checks() {
        let mut t = pr("acme/widget", 42, "Add dark mode");
        t.ci = CiStatus::Failure;
        t.checks = vec![
            CheckRun {
                name: "build".into(),
                status: CiStatus::Failure,
                url: Some("https://ci/build/1".into()),
            },
            CheckRun {
                name: "lint".into(),
                status: CiStatus::Success,
                url: None,
            },
            CheckRun {
                name: "test".into(),
                status: CiStatus::Failure,
                url: None,
            },
        ];
        let p = build_fix_ci_prompt(&t);
        assert!(p.contains("PR #42"));
        assert!(p.contains("acme/widget"));
        assert!(p.contains("feature/x"));
        // Failing checks listed with their URL; the passing one omitted.
        assert!(p.contains("- build — https://ci/build/1"));
        assert!(p.contains("- test"));
        assert!(!p.contains("lint"), "passing checks must not be listed");
        // Tells the agent how to get the logs + to push to the same PR.
        assert!(p.contains("gh pr checks"));
        assert!(p.contains("do NOT open a new one"));
    }

    #[test]
    fn fix_ci_prompt_has_fallback_when_no_per_check_detail() {
        let mut t = pr("o/r", 9, "X");
        t.ci = CiStatus::Failure;
        t.checks = vec![]; // rolled-up Failure but no per-check breakdown
        let p = build_fix_ci_prompt(&t);
        assert!(p.contains("no per-check detail"));
    }

    #[test]
    fn fix_prompts_embed_principles_preamble_before_the_task() {
        // Issue #62 ties into #59: the auto-injected fix prompts must
        // carry the same engineering principles, principles-first.
        let mut t = pr("o/r", 5, "X");
        t.ci = CiStatus::Failure;
        for p in [build_fix_ci_prompt(&t), build_fix_conflict_prompt(&t)] {
            assert!(p.contains(AGENT_WORK_PREAMBLE.trim_end()));
            let scope_at = p.find("## Scope").expect("preamble present");
            let task_at = p.find("## Task").expect("task section present");
            assert!(scope_at < task_at, "principles must precede the task");
        }
    }

    #[test]
    fn fix_conflict_prompt_includes_base_and_merge_instructions() {
        let t = pr("acme/widget", 7, "Refactor");
        let p = build_fix_conflict_prompt(&t);
        assert!(p.contains("PR #7"));
        assert!(p.contains("acme/widget"));
        // Base branch surfaced + the conflict-listing command included.
        assert!(p.contains("main"));
        assert!(p.contains("git diff --name-only --diff-filter=U"));
        // Don't-rewrite-history guidance + same-PR push.
        assert!(p.contains("rewrite the PR's existing commits") || p.contains("Prefer merge"));
        assert!(p.contains("do NOT open a new one"));
    }
}
