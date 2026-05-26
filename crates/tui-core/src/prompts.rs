//! Agent-prompt builders. Pure functions: take a `Workspace` (or a
//! single `Task`) and produce a `(SessionKey, String)` pair the
//! orchestrator can hand to an agent.
//!
//! These used to live in `pilot_tui::components::sidebar`, but
//! `intent::resolve_work` also calls them — and `intent` lives in
//! `pilot-tui-core`, which can't depend on `pilot-tui`. So they're
//! here. Sidebar re-exports them at the old paths for back-compat.

use pilot_core::{SessionKey, Workspace};

/// Polymorphic "work on this" prompt builder. Same priority chain
/// the sidebar's `w` key uses: fix conflicts beats fix CI (CI can't
/// run on an unmergable branch), then implement-issue, otherwise
/// no work to spawn. Lives at module level so the right pane's `w`
/// (which used to blindly address activity rows) can fall back to
/// the same logic when no comments are selected.
pub fn build_work_prompt(workspace: &Workspace) -> Option<(SessionKey, String)> {
    if let Some(target) = build_fix_conflict_prompt(workspace) {
        return Some(target);
    }
    if let Some(target) = build_fix_ci_prompt(workspace) {
        return Some(target);
    }
    // Issue path: workspace's primary task is an issue (no PR
    // linked yet). Once a PR shows up, the merge collapse moves
    // the work to the PR side.
    if workspace.pr.is_some() {
        return None;
    }
    let issue = workspace.gh_issues.first()?;
    let session_key = SessionKey::from(&workspace.key);
    Some((session_key, build_implement_issue_prompt(issue)))
}

/// Pure helper: produce a (session_key, resolve-conflict-prompt)
/// pair if the workspace's PR has merge conflicts with its base;
/// otherwise None. Same shape as [`build_fix_ci_prompt`] so the
/// resolver chain in `intent::resolve_work` can compose them
/// uniformly.
pub fn build_fix_conflict_prompt(workspace: &Workspace) -> Option<(SessionKey, String)> {
    let pr = workspace.pr.as_ref()?;
    if !pr.mergeable.is_conflicting() {
        return None;
    }
    let session_key = SessionKey::from(&workspace.key);
    let pr_number = pr
        .id
        .key
        .rsplit_once('#')
        .map(|(_, n)| n)
        .unwrap_or(&pr.id.key);
    let repo = pr.repo.as_deref().unwrap_or("unknown");
    let branch = pr.branch.as_deref().unwrap_or("unknown");
    let base = pr.base_branch.as_deref().unwrap_or("main");
    let prompt = format!(
        "PR #{pr_number} in {repo} (branch `{branch}`) has merge conflicts with `{base}`. \
         Rebase the branch onto `{base}`, resolve every conflict in-place (read the original \
         intent of both sides before picking — don't blindly favor `--theirs`/`--ours`), \
         run the project's local checks until they pass, then force-push with lease. \
         Reply when the PR is mergeable again."
    );
    Some((session_key, prompt))
}

/// Pure helper: produce a (session_key, fix-CI-prompt) pair if the
/// workspace's PR is currently failing CI; otherwise None. Used by
/// both the sidebar's `w` keymap predicate and `build_work_prompt`.
pub fn build_fix_ci_prompt(workspace: &Workspace) -> Option<(SessionKey, String)> {
    let pr = workspace.pr.as_ref()?;
    if pr.ci != pilot_core::CiStatus::Failure {
        return None;
    }
    let session_key = SessionKey::from(&workspace.key);

    let pr_number = pr
        .id
        .key
        .rsplit_once('#')
        .map(|(_, n)| n)
        .unwrap_or(&pr.id.key);
    let repo = pr.repo.as_deref().unwrap_or("unknown");
    let branch = pr.branch.as_deref().unwrap_or("unknown");
    let failing_checks: Vec<&str> = pr
        .checks
        .iter()
        .filter(|c| c.status == pilot_core::CiStatus::Failure)
        .map(|c| c.name.as_str())
        .collect();
    let checks_block = if failing_checks.is_empty() {
        "Run `gh pr checks` to enumerate the failing checks.".to_string()
    } else {
        format!("Failing checks: {}.", failing_checks.join(", "))
    };
    let prompt = format!(
        "CI is failing on PR #{pr_number} in {repo} (branch `{branch}`). \
         {checks_block} \
         Investigate via `gh pr checks {pr_number}` and `gh run view --log-failed` for each failing run, \
         reproduce the failure locally where possible, fix it, run the relevant local checks until they pass, \
         then commit and `git push`. Reply when CI is green again."
    );
    Some((session_key, prompt))
}

/// Build the agent prompt for `w` ("work on this") when the focused
/// task is a GitHub issue. The agent lands in the issue workspace's
/// worktree with `gh` + `git` available, so the prompt frames the
/// work (issue context + acceptance criteria) and lets the agent
/// handle the branch + PR mechanics.
fn build_implement_issue_prompt(issue: &pilot_core::Task) -> String {
    let issue_number = issue
        .id
        .key
        .rsplit_once('#')
        .map(|(_, n)| n)
        .unwrap_or(&issue.id.key);
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
         checks until they pass, then `gh pr create` with a body that includes \
         `Closes #{issue_number}` so this issue and the resulting PR collapse to \
         a single row in pilot. Reply with the PR URL when it's open.",
        title = issue.title,
    )
}
