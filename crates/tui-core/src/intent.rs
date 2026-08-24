//! Action-to-side-effect resolution.
//!
//! Every user-visible action (Work, Reply, Merge, Adopt, …) is a
//! two-step affair:
//!
//! 1. **Resolve** an `Intent` from the current workspace/pane state
//!    via a pure function in this module. No `&mut self`, no IPC
//!    sending — just `(Workspace, …) -> Intent`. Easy to test: one
//!    line per `(state, action) -> intent` cell.
//! 2. **Execute** the `Intent` in the orchestrator (e.g.,
//!    `Model::execute_intent`). The model holds the side-effect
//!    machinery (IPC client, modal stack, focus); the resolver
//!    doesn't.
//!
//! Why bother: today's `handle_pane_key` mixes both steps in every
//! match arm. The `w`-on-CI-failing-PR bug we shipped a fix for was
//! exactly the kind of thing this split prevents — when "what `w`
//! means" lives in a pure function, the test reads:
//!
//! ```text
//! let intent = resolve_work(Some(&ci_failing_pr), &[], "claude", &lazybox_core::Conventions::default());
//! assert!(matches!(intent, Intent::SpawnAgent { prompt, .. }
//!     if prompt.unwrap().contains("CI is failing")));
//! ```
//!
//! Adding a new action becomes: add a resolver + tests, route it
//! from dispatch, and execute the returned intent. The model itself
//! stays a thin glue layer.

use std::time::Duration;

use lazybox_core::{ActivityFingerprint, Conventions, SessionKey, Workspace, WorkspaceKey};

/// One activity row to persist as read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActivityReadTarget {
    pub index: usize,
    pub fingerprint: Option<ActivityFingerprint>,
}

/// What the model should do in response to an action. Carries the
/// data the side-effect needs (workspace key, prompt text, …) but
/// nothing about *how* to perform it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Intent {
    /// Spawn an agent (Claude / codex / cursor / …) in the named
    /// workspace, optionally pre-loaded with a prompt.
    SpawnAgent {
        workspace_key: SessionKey,
        agent_id: String,
        prompt: Option<String>,
    },
    /// Spawn a plain shell in the named workspace.
    SpawnShell { workspace_key: SessionKey },
    /// Mount the reply textarea targeted at the workspace.
    MountReply { workspace_key: WorkspaceKey },
    /// Mount the new-workspace name input under the given Project.
    /// The model stashes `project_key`, mounts the prompt, and on
    /// submit ships `Command::CreateWorkspace { name, project_key }`.
    MountNewWorkspaceInput {
        project_key: lazybox_core::ProjectKey,
    },
    /// Mount the adopt-target picker for moving sessions out of
    /// the named source workspace.
    MountAdoptPicker { source_key: WorkspaceKey },
    /// Open the focused workspace's worktree in an editor. The
    /// model knows which editor (single → launch directly; multiple
    /// → mount a picker first).
    OpenEditor,
    /// Run the GraphQL `mergePullRequest` mutation for the focused
    /// workspace's PR. Two-press confirm latch is the model's job;
    /// this Intent is the fire-side payload.
    MergePr { workspace_key: WorkspaceKey },
    /// Update the workspace's PR branch against its base — the "Update
    /// branch" button on github.com. Only resolves when the PR is behind
    /// its base; the model ships `Command::UpdateBranch`.
    UpdateBranch { workspace_key: WorkspaceKey },
    /// Flip the workspace's "auto-merge on green" arm and persist it.
    /// `enabled` is the new state (the resolver reads the current flag
    /// and inverts it). The model ships `Command::SetAutoMergeOnGreen`.
    SetAutoMergeOnGreen {
        workspace_key: WorkspaceKey,
        enabled: bool,
    },
    /// Flip the workspace's "track main" arm and persist it (issue #535).
    /// `enabled` is the new state (the resolver reads the current flag
    /// and inverts it). The model ships `Command::SetTrackMain`.
    SetTrackMain {
        workspace_key: WorkspaceKey,
        enabled: bool,
    },
    /// Kill every running terminal under the workspace + remove
    /// the row. Two-press confirm at the model layer.
    KillWorkspace { session_key: SessionKey },
    /// Snooze the workspace until `now + duration`. Producer is
    /// pure (`resolve_short_snooze` / `resolve_long_snooze`); the
    /// The `x z` confirmation flow lives in the model.
    Snooze {
        session_key: SessionKey,
        duration: Duration,
    },
    /// Unsnooze (reset the snoozed-until timestamp). The short-
    /// snooze resolver chooses Snooze vs. Unsnooze based on the
    /// workspace's current state.
    Unsnooze { session_key: SessionKey },
    /// Bulk-mark every activity row on the workspace as read.
    MarkAllRead { session_key: SessionKey },
    /// Mark specific activity rows read. `optimistic` is true for the
    /// focused-cursor path, where the activity pane mirrors the write
    /// immediately and records the undo target.
    MarkActivitiesRead {
        session_key: SessionKey,
        targets: Vec<ActivityReadTarget>,
        optimistic: bool,
        notice: Option<String>,
    },
    /// Fold an issue-only workspace into the PR that claims it.
    CollapseIntoPr { issue_workspace_key: SessionKey },
    /// Mount the handoff target picker with the source agent's captured
    /// output.
    MountHandoffPicker {
        source_key: SessionKey,
        source_name: String,
        seed: String,
        notice: Option<String>,
    },
    /// Show a transient footer notice. Used when an action fires but
    /// can't do anything meaningful in the current state (e.g.,
    /// "no sessions to adopt").
    Notice(String),
    /// The action is not applicable to the current state. Quiet
    /// no-op — no notice, no command. The matching contextual-footer
    /// hint should already not advertise the key.
    NoOp,
}

/// Which branch of the `w` priority chain fires for the given
/// (workspace, selected comments) state. Single classifier so the
/// resolver AND the hint-bar label come from the same source — no
/// hardcoded duplicate strings to drift apart.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkPriority {
    /// User selected comments on an activity row OR the workspace
    /// has unread activity and the viewer is the PR's author /
    /// assignee. Agent gets an "address these comments" prompt:
    /// investigate → reply on GH → push commit.
    AddressComments,
    /// PR has merge conflicts with its base. Agent gets a
    /// "rebase + resolve" prompt. Beats CI fail because CI can't
    /// run cleanly on an unmergable branch.
    FixConflict,
    /// PR's CI is failing. Agent gets a "fix CI" prompt.
    FixCi,
    /// Viewer is the assigned reviewer on a healthy PR. Agent
    /// gets a "review this PR" prompt (walk the diff, leave
    /// inline comments, submit an overall review).
    ReviewCode,
    /// PR with nothing specific flagged (no conflict, green CI, no
    /// unread, viewer isn't the reviewer). `w` is still the default
    /// agent everywhere, so the agent gets a neutral "keep working
    /// on this PR" prompt rather than `w` doing nothing.
    WorkOnPr,
    /// Issue-only workspace (no PR yet). Agent gets an "implement
    /// this issue" prompt.
    ImplementIssue,
    /// Linear-ticket-only workspace (no PR, no GitHub issue). Agent gets
    /// an "implement this Linear ticket" prompt (`Fixes OBI-N` close, the
    /// house branch convention) rather than the bare `StartHere` spawn.
    ImplementLinear,
    /// Empty / scratch workspace (no PR, no issue, not terminal). `w`
    /// still spawns the default agent — bare, no fabricated prompt — so
    /// a blank workspace isn't a silent no-op (issue #557). The old
    /// behavior dropped the keypress with no spawn and no notice.
    StartHere,
    /// The workspace's primary task has reached a terminal lifecycle
    /// state — a merged/closed PR or a closed issue. `w` steers toward
    /// cleanup (a notice pointing at archive) rather than kicking off a
    /// full worktree provision for work that's already done (issue #557,
    /// ties #499 / #552).
    TidyUp,
}

impl WorkPriority {
    /// Short verb for the contextual hint bar. Matches the kind
    /// of work the agent will actually be asked to do, so the
    /// user can predict what `w` will fire before pressing.
    pub fn label(&self) -> &'static str {
        match self {
            Self::AddressComments => "address comments",
            Self::FixConflict => "fix conflict",
            Self::FixCi => "fix CI",
            Self::ReviewCode => "review",
            Self::WorkOnPr => "work on this",
            Self::ImplementIssue => "implement",
            Self::ImplementLinear => "implement",
            Self::StartHere => "start",
            Self::TidyUp => "archive",
        }
    }
}

/// True when the workspace's primary task has reached a terminal
/// lifecycle state (a merged/closed PR or a closed issue). Drives the
/// [`WorkPriority::TidyUp`] steer so `w` nudges toward cleanup instead of
/// provisioning a worktree for finished work (issue #557).
fn workspace_is_terminal(ws: &Workspace) -> bool {
    ws.primary_task().is_some_and(|t| {
        matches!(
            t.state,
            lazybox_core::TaskState::Merged | lazybox_core::TaskState::Closed
        )
    })
}

/// The steering notice `w` shows on a terminal workspace — names the
/// state and the archive chord so the user's next step is one keypress.
fn terminal_steer_message(ws: &Workspace) -> String {
    let merged = ws
        .pr
        .as_ref()
        .is_some_and(|t| t.state == lazybox_core::TaskState::Merged);
    if merged {
        "PR merged — nothing left to work on; press x x to archive this workspace".to_string()
    } else {
        "closed — nothing left to work on; press x x to archive this workspace".to_string()
    }
}

/// Classify what `w` would do on this (workspace, selected-comments)
/// state. `None` means `w` is NoOp — the hint bar should hide it.
/// Used by both `resolve_work` (to build the Intent) and the
/// sidebar's contextual-footer label resolver.
///
/// Priority chain:
/// 1. Explicit selected comments → AddressComments (user is
///    pointing at specific rows).
/// 2. PR + conflicts → FixConflict (blocks everything else).
/// 3. PR + CI failing → FixCi.
/// 4. PR + role=Reviewer (and CI/conflict clean) → ReviewCode
///    (the viewer's job is to read the diff and approve / request
///    changes).
/// 5. PR + role=Author/Assignee + unread activity → AddressComments
///    auto-built from the unread indices. Replies / re-reviews
///    arrived since the user last looked — likely something to act on.
/// 6. Issue-only workspace → ImplementIssue.
pub fn classify_work(
    workspace: Option<&Workspace>,
    selected_comments: &[usize],
) -> Option<WorkPriority> {
    let ws = workspace?;
    if !selected_comments.is_empty() {
        return Some(WorkPriority::AddressComments);
    }
    // A merged/closed PR or closed issue is done — `w` steers to cleanup
    // rather than provisioning a fresh worktree (issue #557). Explicit
    // comment selection above still wins: acting on specific rows is an
    // intentional override.
    if workspace_is_terminal(ws) {
        return Some(WorkPriority::TidyUp);
    }
    if let Some(pr) = ws.pr.as_ref() {
        if pr.mergeable.is_conflicting() {
            return Some(WorkPriority::FixConflict);
        }
        if pr.ci == lazybox_core::CiStatus::Failure {
            return Some(WorkPriority::FixCi);
        }
        // Role-based defaults for healthy PRs. Reviewer always gets
        // ReviewCode — even with no unread, "press w to review" is
        // the natural action when you land on a PR you owe a review
        // on. Author/Assignee get AddressComments when there's
        // something new to look at.
        if pr.role == lazybox_core::TaskRole::Reviewer {
            return Some(WorkPriority::ReviewCode);
        }
        if ws.unread_count() > 0 {
            return Some(WorkPriority::AddressComments);
        }
        // Nothing specific flagged. `w` is the default-agent key on
        // every workspace, so it still fires here with a neutral
        // "keep working on this PR" prompt — never a silent no-op
        // that leaves the hint bar blank and the key unpressable.
        return Some(WorkPriority::WorkOnPr);
    }
    if !ws.gh_issues.is_empty() {
        return Some(WorkPriority::ImplementIssue);
    }
    if !ws.linear_issues.is_empty() {
        return Some(WorkPriority::ImplementLinear);
    }
    // A real but empty/scratch workspace: `w` still spawns the default
    // agent (bare) rather than silently dropping the keypress (issue
    // #557). `None` is now reserved for "no workspace selected at all".
    Some(WorkPriority::StartHere)
}

/// Resolve `w` ("work on this") for a workspace + selected-comment
/// indices. The priority chain lives in `classify_work`; this
/// function turns that classification into a full `Intent` with
/// the right prompt baked in. Both this and the contextual-footer
/// label render off the SAME classifier so they can't drift.
pub fn resolve_work(
    workspace: Option<&Workspace>,
    selected_comments: &[usize],
    agent_id: &str,
    conventions: &Conventions,
) -> Intent {
    let Some(ws) = workspace else {
        return Intent::NoOp;
    };
    let Some(priority) = classify_work(Some(ws), selected_comments) else {
        return Intent::NoOp;
    };
    let session_key = SessionKey::from(&ws.key);
    // A terminal workspace steers to cleanup instead of spawning; a
    // scratch workspace spawns a bare agent (no fabricated prompt).
    let prompt = match priority {
        WorkPriority::TidyUp => return Intent::Notice(terminal_steer_message(ws)),
        WorkPriority::StartHere => None,
        _ => Some(prompt_for_priority(
            ws,
            priority,
            selected_comments,
            conventions,
        )),
    };
    Intent::SpawnAgent {
        workspace_key: session_key,
        agent_id: agent_id.to_string(),
        prompt,
    }
}

/// Turn a `WorkPriority` classification into its agent prompt. TOTAL
/// over any (workspace, priority) pair: `classify_work` normally
/// guarantees the matching builder has data (FixConflict ⇒ a
/// conflicting PR, ImplementIssue ⇒ a gh_issue, …), but that guarantee
/// crosses a function boundary and used to be enforced with `expect`s
/// — a classifier/builder drift then panicked the TUI on `w`, the
/// most-used key. Instead, a priority whose builder finds no data
/// falls back to the generic work prompt (which handles any workspace
/// shape) with a breadcrumb, so drift degrades to a slightly-generic
/// prompt instead of a crash.
fn prompt_for_priority(
    ws: &Workspace,
    priority: WorkPriority,
    selected_comments: &[usize],
    conventions: &Conventions,
) -> String {
    let fallback = |ws: &Workspace| {
        tracing::debug!(
            ?priority,
            workspace = %ws.key,
            "work classification had no prompt data (classifier/builder drift) — \
             falling back to the generic work prompt"
        );
        build_general_pr_prompt(ws)
    };
    match priority {
        WorkPriority::AddressComments => {
            // Explicit selection wins; otherwise auto-fill from
            // the workspace's unread indices (the "you have new
            // comments, address them" path).
            let indices = if selected_comments.is_empty() {
                ws.unread_activity_indices()
            } else {
                selected_comments.to_vec()
            };
            build_address_comments_prompt(ws, &indices)
        }
        WorkPriority::FixConflict => match crate::prompts::build_fix_conflict_prompt(ws) {
            Some((_, prompt)) => prompt,
            None => fallback(ws),
        },
        WorkPriority::FixCi => match crate::prompts::build_fix_ci_prompt(ws) {
            Some((_, prompt)) => prompt,
            None => fallback(ws),
        },
        WorkPriority::ReviewCode => build_review_pr_prompt(ws),
        WorkPriority::WorkOnPr => build_general_pr_prompt(ws),
        WorkPriority::ImplementIssue => match ws.gh_issues.first() {
            Some(issue) => {
                lazybox_core::prompts::build_implement_issue_prompt_with(issue, conventions)
            }
            None => fallback(ws),
        },
        WorkPriority::ImplementLinear => match ws.linear_issues.first() {
            Some(ticket) => {
                lazybox_core::prompts::build_implement_linear_prompt_with(ticket, conventions)
            }
            None => fallback(ws),
        },
        // Handled directly in `resolve_work` (a bare spawn / a steer
        // notice) and never routed through here; kept exhaustive with a
        // safe fallback in case a future caller does.
        WorkPriority::StartHere | WorkPriority::TidyUp => fallback(ws),
    }
}

/// Build the "review this PR" agent prompt. Used when the viewer is
/// the assigned reviewer on a healthy PR — `w` should pre-load the
/// agent with the instruction to walk the diff and submit a review.
fn build_review_pr_prompt(workspace: &Workspace) -> String {
    let (pr_ref, body_block) = workspace
        .pr
        .as_ref()
        .map(|pr| {
            let n = pr
                .id
                .key
                .rsplit_once('#')
                .map(|(_, n)| n)
                .unwrap_or(&pr.id.key);
            let repo = pr.repo.as_deref().unwrap_or("unknown");
            let branch = pr.branch.as_deref().unwrap_or("unknown");
            let r = format!("PR #{n} in {repo} (branch `{branch}`)");
            let body = pr
                .body
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(|s| format!("\n\nPR description:\n{s}\n"))
                .unwrap_or_default();
            (r, body)
        })
        .unwrap_or_else(|| (format!("workspace {}", workspace.key), String::new()));

    format!(
        "Review {pr_ref}. You're listed as a reviewer.\
         {body_block}\n\n\
         Use `gh pr diff` to read the changes against the base branch. \
         For each meaningful concern: leave an inline comment via \
         `gh pr review --comment --body \"...\"` (or `--request-changes` \
         for blockers). When you've walked the whole diff, submit the \
         overall review: `gh pr review --approve` if it looks good, \
         `--request-changes` if there are blockers, or `--comment` if \
         it's nuanced. Be concise — reviewers read review comments, \
         not essays."
    )
}

/// Build the neutral "keep working on this PR" agent prompt. Fires
/// when `w` lands on a PR with nothing specific flagged — the agent
/// is oriented on the PR and told to pick up wherever the work
/// stands, rather than `w` doing nothing.
fn build_general_pr_prompt(workspace: &Workspace) -> String {
    let (pr_ref, body_block) = workspace
        .pr
        .as_ref()
        .map(|pr| {
            let n = pr
                .id
                .key
                .rsplit_once('#')
                .map(|(_, n)| n)
                .unwrap_or(&pr.id.key);
            let repo = pr.repo.as_deref().unwrap_or("unknown");
            let branch = pr.branch.as_deref().unwrap_or("unknown");
            let r = format!("PR #{n} in {repo} (branch `{branch}`)");
            let body = pr
                .body
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(|s| format!("\n\nPR description:\n{s}\n"))
                .unwrap_or_default();
            (r, body)
        })
        .unwrap_or_else(|| (format!("workspace {}", workspace.key), String::new()));

    format!(
        "Continue work on {pr_ref}.{body_block}\n\n\
         Inspect the current state with `gh pr view` and `gh pr diff`, \
         then pick up whatever's outstanding — unfinished implementation, \
         review feedback, or follow-up cleanup. Run the project's local \
         checks until they pass, then commit and push. Reply with a short \
         summary of what changed."
    )
}

/// Resolve `r` (reply). No state-dependent variation — either we
/// have a workspace to reply to or we don't. Kept as a resolver
/// anyway for uniformity: every action has exactly one place its
/// behaviour is defined.
pub fn resolve_reply(workspace: Option<&Workspace>) -> Intent {
    workspace
        .map(|w| Intent::MountReply {
            workspace_key: w.key.clone(),
        })
        .unwrap_or(Intent::NoOp)
}

/// Resolve `e` (open editor). Mirrors `resolve_reply`: present-or-
/// not. The model decides which editor to launch (single → direct,
/// multiple → picker); the resolver just signals "open whatever's
/// configured."
pub fn resolve_open_editor(workspace: Option<&Workspace>) -> Intent {
    if workspace.is_some() {
        Intent::OpenEditor
    } else {
        Intent::NoOp
    }
}

/// Resolve `n` (new workspace). Requires a focused Project — if the
/// cursor isn't on a Project header or on a workspace under one, the
/// resolver returns a `Notice` and the model surfaces a hint instead
/// of mounting the prompt.
pub fn resolve_new_workspace(focused_project_key: Option<lazybox_core::ProjectKey>) -> Intent {
    match focused_project_key {
        Some(project_key) => Intent::MountNewWorkspaceInput { project_key },
        None => Intent::Notice("No project at the cursor — x p picks a repo.".to_string()),
    }
}

/// Resolve `x a` (adopt sessions). Workspace must have at least
/// one session to adopt; otherwise we surface a hint via `Notice`.
pub fn resolve_adopt(workspace: Option<&Workspace>) -> Intent {
    let Some(ws) = workspace else {
        return Intent::NoOp;
    };
    if ws.sessions.is_empty() {
        return Intent::Notice("no sessions on the focused workspace to adopt".into());
    }
    Intent::MountAdoptPicker {
        source_key: ws.key.clone(),
    }
}

/// Resolve `g m` (merge). Gates only on STRUCTURAL facts — the row has
/// an open PR. Cached soft state (CI, review verdicts, mergeability,
/// branch-protection) deliberately does NOT block (#1203): the cache
/// goes stale — hours stale under a rate-limit backoff — and a
/// pre-block on it refused merges GitHub would have accepted. GitHub
/// is the authority at merge time: the daemon ships the mutation and a
/// real rejection comes back as `PrMergeFailed` with GitHub's reason
/// (correcting the cached conflict state as a side effect). ADVISE,
/// NEVER FORBID: the dispatch path surfaces the cached reason as an
/// advisory next to the send, not as a refusal. `merge_block_reason`
/// remains the (stricter, correct) predicate for the READY tag and the
/// no-keypress auto-merge path.
pub fn resolve_merge(workspace: Option<&Workspace>) -> Intent {
    let Some(ws) = workspace else {
        return Intent::NoOp;
    };
    let Some(pr) = ws.pr.as_ref() else {
        return Intent::NoOp;
    };
    if !matches!(
        pr.state,
        lazybox_core::TaskState::Open | lazybox_core::TaskState::InReview
    ) {
        return Intent::NoOp;
    }
    Intent::MergePr {
        workspace_key: ws.key.clone(),
    }
}

/// The cached soft-block advisory for a `g m` send, or `None` when the
/// cache says the PR is merge-ready. Never used to refuse — only to
/// annotate the send ("cached state says X — asking GitHub").
pub fn merge_send_advisory(workspace: Option<&Workspace>) -> Option<&'static str> {
    workspace
        .and_then(|w| w.pr.as_ref())
        .and_then(merge_block_reason)
        .filter(|reason| *reason != "the PR isn't open")
}

/// Resolve `g u` (update branch). Only fires when the workspace's PR is
/// behind its base — the same `BEHIND` signal that drives the status
/// tag — so the action only surfaces where GitHub's "Update branch"
/// button would be live.
pub fn resolve_update_branch(workspace: Option<&Workspace>) -> Intent {
    let Some(ws) = workspace else {
        return Intent::NoOp;
    };
    let Some(pr) = ws.pr.as_ref() else {
        return Intent::NoOp;
    };
    if !pr.is_behind_base {
        return Intent::NoOp;
    }
    Intent::UpdateBranch {
        workspace_key: ws.key.clone(),
    }
}

/// Why this PR can't be merged from lazybox right now. Moved into
/// `lazybox_core::policy` so the daemon's auto-merge path re-verifies
/// with the same predicate; re-exported here so the TUI's gate, footer
/// hint, and existing callers keep one import path.
pub use lazybox_core::policy::merge_block_reason;

/// Resolve the "auto-merge on green" toggle. Flips the workspace's
/// persisted arm. Only meaningful on a workspace that has a PR — an
/// issue-only or empty workspace has nothing to merge, so we surface
/// a `Notice` instead of arming a flag that could never fire.
///
/// Deliberately **config-agnostic**: every eligibility guard — the
/// transient CI / conflict / review states *and* the durable author
/// gate — is evaluated by the daemon, which holds the authoritative
/// `merge_on_green.allow_authors` config. A client (especially a remote
/// `--connect` session whose local config differs) must not pre-judge
/// the author gate here, or it would refuse an arm the daemon would
/// honor. Arming an ineligible PR is not silent: the daemon refuses the
/// author gate at arm time (its `set_auto_merge_on_green`) and surfaces
/// every transient stand-down through the merge attempt.
pub fn resolve_toggle_auto_merge(workspace: Option<&Workspace>) -> Intent {
    let Some(ws) = workspace else {
        return Intent::NoOp;
    };
    if ws.pr.is_none() {
        return Intent::Notice("auto-merge on green applies to a PR".into());
    }
    Intent::SetAutoMergeOnGreen {
        workspace_key: ws.key.clone(),
        enabled: !ws.auto_merge_on_green,
    }
}

/// Resolve the "track main" toggle (issue #535). Only applies to a
/// workspace with a GitHub upstream and a lazybox-provisioned worktree
/// ([`Workspace::supports_track_main`]) — a linked checkout or repo-less
/// row has no `origin/<default>` to fast-forward against, so we surface a
/// `Notice` rather than arming a flag the sweep could never act on.
pub fn resolve_toggle_track_main(workspace: Option<&Workspace>) -> Intent {
    let Some(ws) = workspace else {
        return Intent::NoOp;
    };
    if !ws.supports_track_main() {
        return Intent::Notice("track main applies to a GitHub worktree".into());
    }
    Intent::SetTrackMain {
        workspace_key: ws.key.clone(),
        enabled: !ws.track_main,
    }
}

/// Should a merge auto-fire for this workspace *right now*? Moved into
/// `lazybox_core::policy` — the trigger now lives in the **daemon's**
/// polling commit path (a headless daemon fires it, two attached
/// clients can't double-fire it). Re-exported so the TUI can keep
/// rendering the merge-ready pill from the same predicate.
pub use lazybox_core::policy::should_auto_merge;

/// Resolve `x x` (archive workspace). Always available when a
/// workspace is focused — the model's two-press latch handles the
/// "are you sure" affordance.
pub fn resolve_kill(workspace: Option<&Workspace>) -> Intent {
    workspace
        .map(|w| Intent::KillWorkspace {
            session_key: SessionKey::from(&w.key),
        })
        .unwrap_or(Intent::NoOp)
}

/// Resolve `z` (short snooze). Toggle: if the workspace is already
/// snoozed, unsnooze; otherwise snooze for `duration`. Returns the
/// concrete `Snooze` / `Unsnooze` Intent.
pub fn resolve_short_snooze(
    workspace: Option<&Workspace>,
    now: chrono::DateTime<chrono::Utc>,
    duration: Duration,
) -> Intent {
    let Some(ws) = workspace else {
        return Intent::NoOp;
    };
    let session_key = SessionKey::from(&ws.key);
    if ws.is_snoozed(now) {
        Intent::Unsnooze { session_key }
    } else {
        Intent::Snooze {
            session_key,
            duration,
        }
    }
}

/// Resolve `x z` (long snooze, ~1 year). No toggle behaviour —
/// just snooze for `duration`. The model's `long_snooze_pending`
/// latch handles confirmation.
pub fn resolve_long_snooze(workspace: Option<&Workspace>, duration: Duration) -> Intent {
    workspace
        .map(|w| Intent::Snooze {
            session_key: SessionKey::from(&w.key),
            duration,
        })
        .unwrap_or(Intent::NoOp)
}

/// Resolve `s` (spawn a plain shell). Single-tier: a workspace must
/// be selected. The handler previously did this check inline and
/// dropped the spawn on the floor when no workspace was selected;
/// returning a typed Intent (with explicit `NoOp`) makes the dead
/// branch testable and the contextual footer can hide `s` when no
/// workspace is selected.
pub fn resolve_spawn_shell(workspace: Option<&Workspace>) -> Intent {
    workspace
        .map(|w| Intent::SpawnShell {
            workspace_key: SessionKey::from(&w.key),
        })
        .unwrap_or(Intent::NoOp)
}

/// Resolve an agent shortcut (`a c`/`a x`/`a u` by default — claude/
/// codex/cursor; configurable via `with_agent_shortcuts`). Same shape as
/// the shell resolver: requires a selected workspace.
///
/// The agent id is passed in from the keymap (the resolver doesn't
/// care WHICH agent — that's a presentation/config detail). Empty
/// agent id → `NoOp` so a typo in the config can't silently spawn
/// a bare process.
pub fn resolve_spawn_agent(workspace: Option<&Workspace>, agent_id: &str) -> Intent {
    if agent_id.is_empty() {
        return Intent::NoOp;
    }
    workspace
        .map(|w| Intent::SpawnAgent {
            workspace_key: SessionKey::from(&w.key),
            agent_id: agent_id.to_string(),
            prompt: None,
        })
        .unwrap_or(Intent::NoOp)
}

/// Resolve `m` (mark all read). One-shot.
pub fn resolve_mark_read(workspace: Option<&Workspace>) -> Intent {
    workspace
        .map(|w| Intent::MarkAllRead {
            session_key: SessionKey::from(&w.key),
        })
        .unwrap_or(Intent::NoOp)
}

/// Resolve context-sensitive mark-read semantics from the workspace,
/// activity multi-selection, and optional focused cursor row.
pub fn resolve_mark_read_targets(
    workspace: Option<&Workspace>,
    selected: &[usize],
    cursor: Option<usize>,
) -> Intent {
    let Some(workspace) = workspace else {
        return Intent::NoOp;
    };
    let session_key = SessionKey::from(&workspace.key);
    if !selected.is_empty() {
        let targets = selected
            .iter()
            .map(|index| ActivityReadTarget {
                index: *index,
                fingerprint: workspace.activity.get(*index).map(ActivityFingerprint::of),
            })
            .collect();
        let count = selected.len();
        return Intent::MarkActivitiesRead {
            session_key,
            targets,
            optimistic: false,
            notice: Some(format!(
                "marked {count} selected activit{} read",
                if count == 1 { "y" } else { "ies" }
            )),
        };
    }
    if let Some(index) = cursor {
        let targets = workspace
            .is_activity_unread(index)
            .then(|| ActivityReadTarget {
                index,
                fingerprint: workspace.activity.get(index).map(ActivityFingerprint::of),
            })
            .into_iter()
            .collect();
        return Intent::MarkActivitiesRead {
            session_key,
            targets,
            optimistic: true,
            notice: None,
        };
    }
    Intent::MarkAllRead { session_key }
}

/// Resolve issue-to-PR collapse by finding a locally-synced PR that claims
/// the focused issue.
pub fn resolve_collapse_into_pr(
    issue_workspace: Option<&Workspace>,
    workspaces: &[&Workspace],
) -> Intent {
    let Some(issue_workspace) = issue_workspace else {
        return Intent::NoOp;
    };
    let Some(primary) = issue_workspace.primary_task() else {
        return Intent::NoOp;
    };
    let claiming_pr = workspaces.iter().any(|workspace| {
        workspace
            .pr
            .as_ref()
            .is_some_and(|pr| pr.closes_issues.contains(&primary.id))
    });
    if claiming_pr {
        Intent::CollapseIntoPr {
            issue_workspace_key: SessionKey::from(&issue_workspace.key),
        }
    } else {
        Intent::Notice("no PR closes this issue (or it isn't synced yet)".to_string())
    }
}

/// Resolve an agent-to-agent handoff after the renderer has attempted to
/// capture the focused agent terminal's visible text.
pub fn resolve_send_to_session(
    workspace: Option<&Workspace>,
    captured_seed: Option<String>,
) -> Intent {
    let Some(workspace) = workspace else {
        return Intent::NoOp;
    };
    let Some(seed) = captured_seed else {
        return Intent::Notice("no agent session here to hand off from".to_string());
    };
    let notice = seed
        .is_empty()
        .then(|| "couldn't capture this agent's output — compose the brief yourself".to_string());
    Intent::MountHandoffPicker {
        source_key: SessionKey::from(&workspace.key),
        source_name: workspace.name.clone(),
        seed,
        notice,
    }
}

/// Pending footer copy for an intent whose daemon round-trip is not
/// immediate.
pub fn pending_notice(intent: &Intent, workspace: Option<&Workspace>) -> Option<String> {
    let task_number_suffix = || {
        workspace
            .and_then(|workspace| workspace.pr.as_ref())
            .and_then(|task| task.id.key.rsplit_once('#').map(|(_, number)| number))
            .map(|number| format!(" #{number}"))
            .unwrap_or_default()
    };
    match intent {
        Intent::MergePr { .. } => Some(format!("merging PR{}…", task_number_suffix())),
        Intent::UpdateBranch { .. } => Some(format!("updating branch PR{}…", task_number_suffix())),
        Intent::CollapseIntoPr { .. } => Some("joining into PR…".to_string()),
        _ => None,
    }
}

/// Build the "address these comments" agent prompt. Lifted from
/// `right_pane.rs` so the resolver can call it without the right
/// pane depending on its own internals.
fn build_address_comments_prompt(workspace: &Workspace, indices: &[usize]) -> String {
    let pr_summary = workspace
        .pr
        .as_ref()
        .map(|pr| {
            let n = pr
                .id
                .key
                .rsplit_once('#')
                .map(|(_, n)| n)
                .unwrap_or(&pr.id.key);
            let repo = pr.repo.as_deref().unwrap_or("unknown");
            let branch = pr.branch.as_deref().unwrap_or("unknown");
            format!("PR #{n} in {repo} (branch `{branch}`)")
        })
        .unwrap_or_else(|| format!("workspace {}", workspace.key));

    let mut comments = String::new();
    for (i, idx) in indices.iter().enumerate() {
        let Some(act) = workspace.activity.get(*idx) else {
            continue;
        };
        comments.push_str(&format!(
            "\n[{}] {} on {}:\n",
            i + 1,
            act.author,
            act.created_at
        ));
        if let Some(path) = &act.path {
            if let Some(line) = act.line {
                comments.push_str(&format!("    file: {path}:{line}\n"));
            } else {
                comments.push_str(&format!("    file: {path}\n"));
            }
        }
        for line in act.body.lines() {
            comments.push_str(&format!("    {line}\n"));
        }
    }
    format!(
        "Address the following review comments on {pr_summary}:{comments}\n\n\
         For each comment: investigate, fix the code (or push back with a clear \
         technical rationale), then commit. When all the comments are addressed and \
         local checks pass, push the branch. After the push lands, reply to each \
         comment with the commit SHA and a one-line explanation of the change."
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use lazybox_core::{
        CiStatus, ReviewStatus, Task, TaskId, TaskRole, TaskState, Workspace, WorkspaceKey,
    };

    fn pr(key: &str, ci: CiStatus, review: ReviewStatus) -> Workspace {
        let (path, num) = key.rsplit_once('#').unwrap_or((key, "1"));
        let task = Task {
            author: String::new(),
            id: TaskId {
                source: "github".into(),
                key: key.into(),
            },
            title: format!("PR {key}"),
            body: None,
            state: TaskState::Open,
            role: TaskRole::Author,
            ci,
            review,
            checks: vec![],
            unread_count: 0,
            url: format!("https://github.com/{path}/pull/{num}"),
            repo: Some("o/r".into()),
            branch: Some("main".into()),
            base_branch: None,
            updated_at: Utc::now(),
            created_at: None,
            closed_at: None,
            labels: vec![],
            reviewers: vec![],
            reviews: vec![],
            assignees: vec![],
            auto_merge_enabled: false,
            is_in_merge_queue: false,
            mergeable: lazybox_core::Mergeable::mergeable(),
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
        };
        Workspace::from_task(task, Utc::now())
    }

    fn issue(key: &str) -> Workspace {
        let (path, num) = key.rsplit_once('#').unwrap_or((key, "1"));
        let mut t = pr(key, CiStatus::None, ReviewStatus::None);
        // Convert to issue: clear pr, attach as gh_issue.
        let mut task = t.pr.take().unwrap();
        task.url = format!("https://github.com/{path}/issues/{num}");
        t.attach_task(task);
        t
    }

    fn empty() -> Workspace {
        Workspace::empty(WorkspaceKey::new("k"), "main", Utc::now())
    }

    fn linear(key: &str) -> Workspace {
        let mut t = pr(&format!("o/r#{key}"), CiStatus::None, ReviewStatus::None);
        let mut task = t.pr.take().unwrap();
        task.id = TaskId {
            source: "linear".into(),
            key: key.into(),
        };
        task.repo = Some(format!(
            "linear/{}",
            key.split_once('-').map_or(key, |(t, _)| t)
        ));
        task.branch = None;
        task.url = format!("https://linear.app/team/issue/{key}");
        t.attach_task(task);
        t
    }

    #[test]
    fn work_with_no_workspace_is_noop() {
        assert_eq!(
            resolve_work(None, &[], "claude", &lazybox_core::Conventions::default()),
            Intent::NoOp
        );
    }

    // ── classifier/builder drift must not panic ───────────────────
    //
    // `classify_work` and the per-priority prompt builders encode the
    // same predicates in two places. Should they ever drift (a future
    // refactor loosens one side), pressing `w` must degrade to the
    // generic prompt — never panic the TUI. The states below are
    // unreachable through today's classifier, which is exactly the
    // point: this pins the non-panic CONTRACT of the resolution step
    // independently of the classifier's current behavior.
    #[test]
    fn drifted_fix_ci_classification_falls_back_to_generic_prompt() {
        // A workspace with no PR at all can never satisfy
        // build_fix_ci_prompt — the drifted FixCi must fall back.
        let ws = empty();
        let prompt = prompt_for_priority(
            &ws,
            WorkPriority::FixCi,
            &[],
            &lazybox_core::Conventions::default(),
        );
        assert!(
            prompt.contains("Continue work on"),
            "expected the generic work prompt, got: {prompt}"
        );
    }

    #[test]
    fn drifted_fix_conflict_classification_falls_back_to_generic_prompt() {
        // PR present but NOT conflicting: build_fix_conflict_prompt
        // returns None even though the (drifted) classification says
        // FixConflict.
        let ws = pr("o/r#1", CiStatus::Success, ReviewStatus::None);
        let prompt = prompt_for_priority(
            &ws,
            WorkPriority::FixConflict,
            &[],
            &lazybox_core::Conventions::default(),
        );
        assert!(
            prompt.contains("Continue work on"),
            "expected the generic work prompt, got: {prompt}"
        );
    }

    #[test]
    fn drifted_implement_issue_classification_falls_back_to_generic_prompt() {
        // No gh_issues on the workspace — the drifted ImplementIssue
        // must fall back instead of expecting `.first()`.
        let ws = empty();
        let prompt = prompt_for_priority(
            &ws,
            WorkPriority::ImplementIssue,
            &[],
            &lazybox_core::Conventions::default(),
        );
        assert!(
            prompt.contains("Continue work on"),
            "expected the generic work prompt, got: {prompt}"
        );
    }

    #[test]
    fn interactive_work_honors_configured_conventions() {
        // #3: the interactive `w w` path must inject the user's
        // conventions into the issue brief, not silently use defaults.
        let ws = issue("o/r#7");
        let conv = lazybox_core::Conventions {
            commit_style: lazybox_core::CommitStyle::Custom,
            custom_instruction: Some("Gitmoji prefixes on every commit".into()),
            ..Default::default()
        };
        let intent = resolve_work(Some(&ws), &[], "claude", &conv);
        let Intent::SpawnAgent { prompt, .. } = intent else {
            panic!("expected SpawnAgent");
        };
        let prompt = prompt.expect("issue work carries a prompt");
        assert!(
            prompt.contains("Gitmoji prefixes on every commit"),
            "interactive brief must honor configured conventions"
        );
        // Default conventions leave the brief on the built-in Conventional
        // Commits guidance (no override paragraph).
        let default_prompt = match resolve_work(
            Some(&ws),
            &[],
            "claude",
            &lazybox_core::Conventions::default(),
        ) {
            Intent::SpawnAgent { prompt, .. } => prompt.unwrap(),
            _ => panic!("expected SpawnAgent"),
        };
        assert!(!default_prompt.contains("Gitmoji"));
        assert!(default_prompt.contains("Conventional Commits"));
    }

    #[test]
    fn work_on_ci_failing_pr_returns_fix_ci_agent() {
        let ws = pr("o/r#1", CiStatus::Failure, ReviewStatus::Pending);
        let intent = resolve_work(
            Some(&ws),
            &[],
            "claude",
            &lazybox_core::Conventions::default(),
        );
        match intent {
            Intent::SpawnAgent {
                agent_id, prompt, ..
            } => {
                assert_eq!(agent_id, "claude");
                let prompt = prompt.expect("fix-CI carries a prompt");
                assert!(prompt.contains("CI is failing"), "{prompt}",);
            }
            other => panic!("expected SpawnAgent, got {other:?}"),
        }
    }

    #[test]
    fn work_on_conflict_pr_returns_resolve_conflict_agent() {
        // Merge conflict surfaces as `mergeable=Conflicting`. `w` must
        // fire — without this, the user sits on a CONFLICT-pill row
        // and the hint bar shows nothing under `w`.
        let mut ws = pr("o/r#7", CiStatus::None, ReviewStatus::None);
        ws.pr.as_mut().unwrap().mergeable = lazybox_core::Mergeable::conflicting();
        let intent = resolve_work(
            Some(&ws),
            &[],
            "claude",
            &lazybox_core::Conventions::default(),
        );
        match intent {
            Intent::SpawnAgent { prompt, .. } => {
                let prompt = prompt.expect("conflict-fix carries a prompt");
                assert!(
                    prompt.contains("merge conflicts"),
                    "conflict prompt must mention conflicts; got:\n{prompt}",
                );
                assert!(
                    prompt.contains("Rebase"),
                    "conflict prompt must direct a rebase; got:\n{prompt}",
                );
            }
            other => panic!("expected SpawnAgent, got {other:?}"),
        }
    }

    #[test]
    fn conflict_beats_ci_fail_when_both_apply() {
        // A conflicted branch can't run CI cleanly — fix the
        // conflict first. Pin the priority so a future refactor
        // doesn't accidentally swap the order.
        let mut ws = pr("o/r#7", CiStatus::Failure, ReviewStatus::None);
        ws.pr.as_mut().unwrap().mergeable = lazybox_core::Mergeable::conflicting();
        let intent = resolve_work(
            Some(&ws),
            &[],
            "claude",
            &lazybox_core::Conventions::default(),
        );
        match intent {
            Intent::SpawnAgent { prompt, .. } => {
                let prompt = prompt.expect("carries prompt");
                assert!(
                    prompt.contains("merge conflicts"),
                    "conflict must win over CI fail when both apply; got:\n{prompt}",
                );
            }
            other => panic!("expected SpawnAgent, got {other:?}"),
        }
    }

    // ── classify_work / resolve_work consistency ──────────────────
    //
    // Both the resolver (builds Intent) and the hint-bar label
    // resolver consult `classify_work`. Pin that they ALWAYS agree:
    // any state that classify_work classifies must produce a
    // SpawnAgent from resolve_work, and any state classified as
    // None must produce NoOp.

    #[test]
    fn classify_and_resolve_agree_on_every_canonical_state() {
        let cases: Vec<(&str, Option<WorkPriority>, Workspace, &[usize])> = {
            let healthy_pr = pr("o/r#1", CiStatus::Success, ReviewStatus::Pending);
            let ci_fail = pr("o/r#1", CiStatus::Failure, ReviewStatus::Pending);
            let mut conflict_pr = pr("o/r#7", CiStatus::None, ReviewStatus::None);
            conflict_pr.pr.as_mut().unwrap().mergeable = lazybox_core::Mergeable::conflicting();
            let mut conflict_plus_ci = pr("o/r#8", CiStatus::Failure, ReviewStatus::None);
            conflict_plus_ci.pr.as_mut().unwrap().mergeable =
                lazybox_core::Mergeable::conflicting();
            let issue = issue("o/r#42");
            let mut commented = pr("o/r#9", CiStatus::Success, ReviewStatus::Pending);
            commented.activity.push(lazybox_core::Activity {
                author: "alice".into(),
                body: "comment".into(),
                created_at: Utc::now(),
                kind: lazybox_core::ActivityKind::Comment,
                node_id: None,
                path: None,
                line: None,
                diff_hunk: None,
                thread_id: None,
            });
            vec![
                (
                    "healthy PR",
                    Some(WorkPriority::WorkOnPr),
                    healthy_pr,
                    &[][..],
                ),
                ("ci-fail PR", Some(WorkPriority::FixCi), ci_fail, &[][..]),
                (
                    "conflict PR",
                    Some(WorkPriority::FixConflict),
                    conflict_pr,
                    &[][..],
                ),
                (
                    "conflict beats ci",
                    Some(WorkPriority::FixConflict),
                    conflict_plus_ci,
                    &[][..],
                ),
                ("issue", Some(WorkPriority::ImplementIssue), issue, &[][..]),
                (
                    "linear ticket",
                    Some(WorkPriority::ImplementLinear),
                    linear("OBI-1749"),
                    &[][..],
                ),
                (
                    "comments selected",
                    Some(WorkPriority::AddressComments),
                    commented,
                    &[0][..],
                ),
                (
                    "empty workspace",
                    Some(WorkPriority::StartHere),
                    empty(),
                    &[][..],
                ),
            ]
        };

        for (name, expected, ws, comments) in cases {
            let classified = classify_work(Some(&ws), comments);
            assert_eq!(
                classified, expected,
                "classify_work mismatch for `{name}`: got {classified:?}, expected {expected:?}",
            );
            let intent = resolve_work(
                Some(&ws),
                comments,
                "claude",
                &lazybox_core::Conventions::default(),
            );
            match (classified, &intent) {
                // StartHere spawns a bare agent (prompt None); the rest
                // carry a prompt — both are `SpawnAgent`.
                (Some(_), Intent::SpawnAgent { .. }) => {}
                (None, Intent::NoOp) => {}
                _ => panic!(
                    "resolve_work / classify_work disagree for `{name}`: \
                     classify={classified:?}, intent={intent:?}",
                ),
            }
        }
    }

    #[test]
    fn work_priority_labels_are_short_and_present_tense() {
        // Hint-bar real estate is tight — labels must stay short.
        // Pin so a future label change has to update the test too.
        for p in [
            WorkPriority::AddressComments,
            WorkPriority::FixConflict,
            WorkPriority::FixCi,
            WorkPriority::ReviewCode,
            WorkPriority::WorkOnPr,
            WorkPriority::ImplementIssue,
            WorkPriority::ImplementLinear,
            WorkPriority::StartHere,
            WorkPriority::TidyUp,
        ] {
            let label = p.label();
            assert!(!label.is_empty(), "{p:?} label is empty");
            assert!(
                label.len() <= 18,
                "{p:?} label `{label}` is too long for the hint bar",
            );
        }
    }

    #[test]
    fn selected_comments_beat_conflict() {
        // The comments path is most-explicit user intent: they
        // selected what to address. Conflict / CI fall behind.
        let mut ws = pr("o/r#7", CiStatus::None, ReviewStatus::None);
        ws.pr.as_mut().unwrap().mergeable = lazybox_core::Mergeable::conflicting();
        ws.activity.push(lazybox_core::Activity {
            author: "alice".into(),
            body: "fix the lint please".into(),
            created_at: Utc::now(),
            kind: lazybox_core::ActivityKind::Comment,
            node_id: None,
            path: None,
            line: None,
            diff_hunk: None,
            thread_id: None,
        });
        let intent = resolve_work(
            Some(&ws),
            &[0],
            "claude",
            &lazybox_core::Conventions::default(),
        );
        match intent {
            Intent::SpawnAgent { prompt, .. } => {
                let prompt = prompt.expect("carries prompt");
                assert!(
                    prompt.contains("Address the following review comments"),
                    "selected comments must beat conflict; got:\n{prompt}",
                );
            }
            other => panic!("expected SpawnAgent, got {other:?}"),
        }
    }

    #[test]
    fn work_on_healthy_pr_spawns_default_agent() {
        // `w` is the default-agent key on every workspace: a healthy
        // PR with nothing flagged still fires, with a neutral
        // "keep working on this PR" prompt — never a silent no-op.
        let ws = pr("o/r#1", CiStatus::Success, ReviewStatus::Pending);
        match resolve_work(
            Some(&ws),
            &[],
            "claude",
            &lazybox_core::Conventions::default(),
        ) {
            Intent::SpawnAgent { prompt, .. } => {
                let prompt = prompt.expect("work-on-PR carries a prompt");
                assert!(prompt.contains("Continue work on"), "{prompt}");
            }
            other => panic!("expected SpawnAgent, got {other:?}"),
        }
    }

    #[test]
    fn work_on_ready_pr_spawns_default_agent() {
        // READY (approved + green) surfaces Merge as the primary
        // footer action, but `w` must still launch the default agent
        // when pressed — the user expects it to work from every PR.
        let ws = pr("o/r#1", CiStatus::Success, ReviewStatus::Approved);
        match resolve_work(
            Some(&ws),
            &[],
            "claude",
            &lazybox_core::Conventions::default(),
        ) {
            Intent::SpawnAgent { .. } => {}
            other => panic!("expected SpawnAgent, got {other:?}"),
        }
    }

    #[test]
    fn work_on_issue_returns_implement_agent() {
        let ws = issue("o/r#42");
        let intent = resolve_work(
            Some(&ws),
            &[],
            "claude",
            &lazybox_core::Conventions::default(),
        );
        match intent {
            Intent::SpawnAgent { prompt, .. } => {
                let prompt = prompt.expect("implement carries a prompt");
                assert!(prompt.contains("Implement GitHub issue #42"), "{prompt}",);
            }
            other => panic!("expected SpawnAgent, got {other:?}"),
        }
    }

    /// Issue #557: `w` on an empty/scratch workspace no longer silently
    /// drops the keypress — it spawns the default agent, bare (no
    /// fabricated PR/issue prompt). Only "no workspace selected at all"
    /// remains a NoOp.
    #[test]
    fn work_on_empty_workspace_spawns_a_bare_agent() {
        assert_eq!(
            classify_work(Some(&empty()), &[]),
            Some(WorkPriority::StartHere)
        );
        match resolve_work(
            Some(&empty()),
            &[],
            "claude",
            &lazybox_core::Conventions::default(),
        ) {
            Intent::SpawnAgent {
                agent_id, prompt, ..
            } => {
                assert_eq!(agent_id, "claude");
                assert_eq!(prompt, None, "scratch spawn carries no fabricated prompt");
            }
            other => panic!("expected a bare SpawnAgent, got {other:?}"),
        }
    }

    /// Issue #557: `w` on a merged PR steers to cleanup (a notice
    /// pointing at the archive chord) instead of provisioning a worktree
    /// for work that's already done.
    #[test]
    fn work_on_merged_pr_steers_to_archive() {
        let mut ws = pr("o/r#7", CiStatus::Success, ReviewStatus::Approved);
        ws.pr.as_mut().expect("pr present").state = TaskState::Merged;
        assert_eq!(classify_work(Some(&ws), &[]), Some(WorkPriority::TidyUp));
        match resolve_work(
            Some(&ws),
            &[],
            "claude",
            &lazybox_core::Conventions::default(),
        ) {
            Intent::Notice(msg) => {
                assert!(msg.contains("merged"), "{msg}");
                assert!(msg.contains("x x"), "names the archive chord: {msg}");
            }
            other => panic!("expected a steering Notice, got {other:?}"),
        }
    }

    /// A closed issue likewise steers to cleanup rather than spawning an
    /// "implement" agent on finished work.
    #[test]
    fn work_on_closed_issue_steers_to_archive() {
        let mut ws = issue("o/r#8");
        ws.gh_issues[0].state = TaskState::Closed;
        assert_eq!(classify_work(Some(&ws), &[]), Some(WorkPriority::TidyUp));
        assert!(matches!(
            resolve_work(
                Some(&ws),
                &[],
                "claude",
                &lazybox_core::Conventions::default()
            ),
            Intent::Notice(_)
        ));
    }

    /// Explicit comment selection is an intentional override: acting on
    /// specific rows still spawns even on a merged PR.
    #[test]
    fn selected_comments_override_the_terminal_steer() {
        let mut ws = pr("o/r#9", CiStatus::Success, ReviewStatus::Approved);
        ws.pr.as_mut().expect("pr present").state = TaskState::Merged;
        ws.activity.push(lazybox_core::Activity {
            author: "alice".into(),
            body: "one more thing".into(),
            created_at: Utc::now(),
            kind: lazybox_core::ActivityKind::Comment,
            node_id: None,
            path: None,
            line: None,
            diff_hunk: None,
            thread_id: None,
        });
        assert_eq!(
            classify_work(Some(&ws), &[0]),
            Some(WorkPriority::AddressComments)
        );
        assert!(matches!(
            resolve_work(
                Some(&ws),
                &[0],
                "claude",
                &lazybox_core::Conventions::default()
            ),
            Intent::SpawnAgent { .. }
        ));
    }

    #[test]
    fn selected_comments_beat_ci_failure() {
        // Comments-selected path wins even when CI is red — the user
        // explicitly chose what to address.
        let mut ws = pr("o/r#1", CiStatus::Failure, ReviewStatus::Pending);
        ws.activity.push(lazybox_core::Activity {
            author: "alice".into(),
            body: "needs more tests".into(),
            created_at: Utc::now(),
            kind: lazybox_core::ActivityKind::Comment,
            node_id: None,
            path: None,
            line: None,
            diff_hunk: None,
            thread_id: None,
        });
        let intent = resolve_work(
            Some(&ws),
            &[0],
            "claude",
            &lazybox_core::Conventions::default(),
        );
        match intent {
            Intent::SpawnAgent { prompt, .. } => {
                let prompt = prompt.expect("carries prompt");
                assert!(
                    prompt.contains("Address the following review comments"),
                    "selected comments must beat fix-CI; got:\n{prompt}",
                );
            }
            other => panic!("expected SpawnAgent, got {other:?}"),
        }
    }

    // ── Reply ────────────────────────────────────────────────────

    #[test]
    fn reply_with_no_workspace_is_noop() {
        assert_eq!(resolve_reply(None), Intent::NoOp);
    }

    #[test]
    fn reply_with_workspace_mounts_reply() {
        let ws = pr("o/r#1", CiStatus::None, ReviewStatus::None);
        match resolve_reply(Some(&ws)) {
            Intent::MountReply { workspace_key } => assert_eq!(workspace_key, ws.key),
            other => panic!("expected MountReply, got {other:?}"),
        }
    }

    // ── Open editor ──────────────────────────────────────────────

    #[test]
    fn open_editor_with_no_workspace_is_noop() {
        assert_eq!(resolve_open_editor(None), Intent::NoOp);
    }

    #[test]
    fn open_editor_with_workspace_returns_open_editor() {
        let ws = pr("o/r#1", CiStatus::None, ReviewStatus::None);
        assert_eq!(resolve_open_editor(Some(&ws)), Intent::OpenEditor);
    }

    // ── New workspace ────────────────────────────────────────────

    #[test]
    fn new_workspace_with_project_mounts_input() {
        let pk = lazybox_core::ProjectKey::local("my-project");
        assert_eq!(
            resolve_new_workspace(Some(pk.clone())),
            Intent::MountNewWorkspaceInput { project_key: pk }
        );
    }

    #[test]
    fn new_workspace_without_project_returns_notice() {
        match resolve_new_workspace(None) {
            Intent::Notice(msg) => assert!(msg.to_lowercase().contains("project")),
            other => panic!("expected Notice, got {other:?}"),
        }
    }

    // ── Adopt sessions ───────────────────────────────────────────

    #[test]
    fn adopt_with_no_workspace_is_noop() {
        assert_eq!(resolve_adopt(None), Intent::NoOp);
    }

    #[test]
    fn adopt_with_empty_workspace_surfaces_notice() {
        let ws = pr("o/r#1", CiStatus::None, ReviewStatus::None);
        match resolve_adopt(Some(&ws)) {
            Intent::Notice(msg) => assert!(msg.contains("no sessions"), "{msg}"),
            other => panic!("expected Notice, got {other:?}"),
        }
    }

    #[test]
    fn adopt_with_sessions_mounts_picker() {
        let mut ws = pr("o/r#1", CiStatus::None, ReviewStatus::None);
        ws.add_session(lazybox_core::WorkspaceSession::new(
            ws.key.clone(),
            lazybox_core::SessionKind::Shell,
            std::path::PathBuf::from("/tmp"),
            Utc::now(),
        ));
        match resolve_adopt(Some(&ws)) {
            Intent::MountAdoptPicker { source_key } => assert_eq!(source_key, ws.key),
            other => panic!("expected MountAdoptPicker, got {other:?}"),
        }
    }

    // ── Update branch ────────────────────────────────────────────

    #[test]
    fn update_branch_on_behind_pr_returns_intent() {
        let mut ws = pr("o/r#1", CiStatus::Success, ReviewStatus::Approved);
        ws.pr.as_mut().unwrap().is_behind_base = true;
        match resolve_update_branch(Some(&ws)) {
            Intent::UpdateBranch { workspace_key } => assert_eq!(workspace_key, ws.key),
            other => panic!("expected UpdateBranch, got {other:?}"),
        }
    }

    #[test]
    fn update_branch_on_up_to_date_pr_is_noop() {
        let ws = pr("o/r#1", CiStatus::Success, ReviewStatus::Approved);
        assert!(!ws.pr.as_ref().unwrap().is_behind_base);
        assert_eq!(resolve_update_branch(Some(&ws)), Intent::NoOp);
    }

    #[test]
    fn update_branch_on_issue_or_none_is_noop() {
        assert_eq!(resolve_update_branch(None), Intent::NoOp);
        let issue = issue("o/r#2");
        assert_eq!(resolve_update_branch(Some(&issue)), Intent::NoOp);
    }

    // ── Merge ────────────────────────────────────────────────────

    #[test]
    fn merge_on_ready_pr_returns_merge_intent() {
        let ws = pr("o/r#1", CiStatus::Success, ReviewStatus::Approved);
        match resolve_merge(Some(&ws)) {
            Intent::MergePr { workspace_key } => assert_eq!(workspace_key, ws.key),
            other => panic!("expected MergePr, got {other:?}"),
        }
    }

    #[test]
    fn merge_without_approval_still_merges_on_green_ci() {
        // Repos without required reviews (your own PR, a solo repo)
        // have no formal Approved review, but GitHub merges them
        // immediately — so green CI + no approval must NOT block.
        for review in [ReviewStatus::None, ReviewStatus::Pending] {
            let ws = pr("o/r#1", CiStatus::Success, review);
            match resolve_merge(Some(&ws)) {
                Intent::MergePr { workspace_key } => assert_eq!(workspace_key, ws.key),
                other => panic!("expected MergePr for {review:?}, got {other:?}"),
            }
            assert_eq!(merge_block_reason(ws.pr.as_ref().unwrap()), None);
        }
    }

    /// #1203: cached soft state advises, it never refuses. All three
    /// soft blocks — changes requested, red CI, a cached conflict —
    /// still resolve to a merge send (GitHub is the authority and its
    /// rejection comes back with the real reason); the cached reason
    /// rides along as the send advisory instead.
    #[test]
    fn merge_with_changes_requested_sends_with_advisory() {
        let ws = pr("o/r#1", CiStatus::Success, ReviewStatus::ChangesRequested);
        assert!(matches!(resolve_merge(Some(&ws)), Intent::MergePr { .. }));
        assert_eq!(
            merge_send_advisory(Some(&ws)),
            Some("changes were requested — address the review first")
        );
    }

    #[test]
    fn merge_with_red_ci_sends_with_advisory() {
        let ws = pr("o/r#1", CiStatus::Failure, ReviewStatus::Approved);
        assert!(matches!(resolve_merge(Some(&ws)), Intent::MergePr { .. }));
        assert_eq!(merge_send_advisory(Some(&ws)), Some("CI isn't green yet"));
    }

    #[test]
    fn merge_with_cached_conflict_sends_with_advisory() {
        let mut ws = pr("o/r#1", CiStatus::Success, ReviewStatus::None);
        ws.pr.as_mut().unwrap().mergeable = lazybox_core::Mergeable::conflicting();
        assert!(matches!(resolve_merge(Some(&ws)), Intent::MergePr { .. }));
        assert_eq!(
            merge_send_advisory(Some(&ws)),
            Some("the branch has merge conflicts")
        );
    }

    /// The stale-cache journey #1203 reported: mergeability `Unknown`
    /// (not yet computed — common under a rate-limit backoff) must not
    /// block, and a MERGED/CLOSED PR is the only structural no-op.
    #[test]
    fn merge_with_unknown_mergeability_sends_and_closed_pr_noops() {
        let mut ws = pr("o/r#1", CiStatus::Success, ReviewStatus::Approved);
        ws.pr.as_mut().unwrap().mergeable = lazybox_core::Mergeable::unknown();
        assert!(matches!(resolve_merge(Some(&ws)), Intent::MergePr { .. }));

        let mut merged = pr("o/r#2", CiStatus::Success, ReviewStatus::Approved);
        merged.pr.as_mut().unwrap().state = lazybox_core::TaskState::Merged;
        assert_eq!(resolve_merge(Some(&merged)), Intent::NoOp);
        assert_eq!(
            merge_send_advisory(Some(&merged)),
            None,
            "'isn't open' is structural, not an advisory"
        );
    }

    #[test]
    fn merge_on_issue_is_noop() {
        let ws = issue("o/r#42");
        assert_eq!(resolve_merge(Some(&ws)), Intent::NoOp);
    }

    // ── Auto-merge toggle + trigger ──────────────────────────────

    #[test]
    fn toggle_auto_merge_flips_the_arm() {
        let mut ws = pr("o/r#1", CiStatus::Success, ReviewStatus::None);
        assert!(!ws.auto_merge_on_green);
        match resolve_toggle_auto_merge(Some(&ws)) {
            Intent::SetAutoMergeOnGreen {
                workspace_key,
                enabled,
            } => {
                assert_eq!(workspace_key, ws.key);
                assert!(enabled, "arming a disarmed workspace enables it");
            }
            other => panic!("expected SetAutoMergeOnGreen, got {other:?}"),
        }
        ws.auto_merge_on_green = true;
        match resolve_toggle_auto_merge(Some(&ws)) {
            Intent::SetAutoMergeOnGreen { enabled, .. } => {
                assert!(!enabled, "toggling an armed workspace disarms it");
            }
            other => panic!("expected SetAutoMergeOnGreen, got {other:?}"),
        }
    }

    /// The toggle is deliberately config-agnostic: even a third party's
    /// PR arms here (the daemon's `set_auto_merge_on_green` owns the
    /// author-gate refusal against its authoritative config, so a remote
    /// client can't wrongly pre-refuse it — issue #845).
    #[test]
    fn toggle_auto_merge_arms_a_non_own_pr_leaving_the_gate_to_the_daemon() {
        let mut ws = pr("o/r#1", CiStatus::Success, ReviewStatus::None);
        ws.pr.as_mut().unwrap().role = TaskRole::Reviewer;
        ws.pr.as_mut().unwrap().author = "dependabot[bot]".into();
        match resolve_toggle_auto_merge(Some(&ws)) {
            Intent::SetAutoMergeOnGreen { enabled, .. } => {
                assert!(enabled, "arming is client-config-agnostic")
            }
            other => panic!("expected SetAutoMergeOnGreen, got {other:?}"),
        }
    }

    #[test]
    fn toggle_auto_merge_on_issue_surfaces_notice() {
        let ws = issue("o/r#42");
        match resolve_toggle_auto_merge(Some(&ws)) {
            Intent::Notice(msg) => assert!(msg.contains("PR"), "{msg}"),
            other => panic!("expected Notice, got {other:?}"),
        }
    }

    #[test]
    fn toggle_auto_merge_no_workspace_is_noop() {
        assert_eq!(resolve_toggle_auto_merge(None), Intent::NoOp);
    }

    // ── Track-main toggle ────────────────────────────────────────

    /// A persistent scratch workspace under a GitHub project — no PR, no
    /// linked checkout — the shape track-main is built for.
    fn scratch() -> Workspace {
        let mut ws = Workspace::empty(WorkspaceKey::new("scratch"), "scratch", Utc::now());
        ws.project_key = Some(lazybox_core::ProjectKey::github("acme", "widgets"));
        ws
    }

    #[test]
    fn toggle_track_main_flips_the_arm() {
        let mut ws = scratch();
        assert!(ws.supports_track_main());
        assert!(!ws.track_main);
        match resolve_toggle_track_main(Some(&ws)) {
            Intent::SetTrackMain {
                workspace_key,
                enabled,
            } => {
                assert_eq!(workspace_key, ws.key);
                assert!(enabled, "arming a disarmed workspace enables it");
            }
            other => panic!("expected SetTrackMain, got {other:?}"),
        }
        ws.track_main = true;
        match resolve_toggle_track_main(Some(&ws)) {
            Intent::SetTrackMain { enabled, .. } => {
                assert!(!enabled, "toggling an armed workspace disarms it");
            }
            other => panic!("expected SetTrackMain, got {other:?}"),
        }
    }

    #[test]
    fn toggle_track_main_on_unsupported_workspace_surfaces_notice() {
        // A repo-less scratch workspace has no origin to track.
        let ws = empty();
        assert!(!ws.supports_track_main());
        match resolve_toggle_track_main(Some(&ws)) {
            Intent::Notice(msg) => assert!(msg.contains("GitHub"), "{msg}"),
            other => panic!("expected Notice, got {other:?}"),
        }
    }

    #[test]
    fn toggle_track_main_on_pr_workspace_surfaces_notice() {
        // A PR branch is ahead of AND behind main — a fast-forward can
        // never apply, so track-main is not offered there.
        let ws = pr("o/r#1", CiStatus::Success, ReviewStatus::None);
        assert!(ws.pr.is_some());
        assert!(!ws.supports_track_main());
        match resolve_toggle_track_main(Some(&ws)) {
            Intent::Notice(_) => {}
            other => panic!("expected Notice, got {other:?}"),
        }
    }

    #[test]
    fn toggle_track_main_no_workspace_is_noop() {
        assert_eq!(resolve_toggle_track_main(None), Intent::NoOp);
    }

    // The `should_auto_merge` guard tests moved to
    // `lazybox_core::policy::merge_gate_tests` alongside the predicate
    // (the daemon's polling path is the trigger now).

    // ── Kill ─────────────────────────────────────────────────────

    #[test]
    fn kill_with_no_workspace_is_noop() {
        assert_eq!(resolve_kill(None), Intent::NoOp);
    }

    #[test]
    fn kill_with_workspace_returns_kill_intent() {
        let ws = pr("o/r#1", CiStatus::None, ReviewStatus::None);
        match resolve_kill(Some(&ws)) {
            Intent::KillWorkspace { session_key } => {
                assert_eq!(session_key.as_str(), ws.key.as_str());
            }
            other => panic!("expected KillWorkspace, got {other:?}"),
        }
    }

    // ── Snooze (short) ───────────────────────────────────────────

    #[test]
    fn short_snooze_with_no_workspace_is_noop() {
        assert_eq!(
            resolve_short_snooze(None, Utc::now(), Duration::from_secs(4 * 3600)),
            Intent::NoOp
        );
    }

    #[test]
    fn short_snooze_on_fresh_workspace_snoozes() {
        let ws = pr("o/r#1", CiStatus::None, ReviewStatus::None);
        let d = Duration::from_secs(4 * 3600);
        match resolve_short_snooze(Some(&ws), Utc::now(), d) {
            Intent::Snooze { duration, .. } => assert_eq!(duration, d),
            other => panic!("expected Snooze, got {other:?}"),
        }
    }

    #[test]
    fn short_snooze_on_already_snoozed_workspace_unsnoozes() {
        // Toggle behavior: pressing `z` twice undoes the snooze.
        let mut ws = pr("o/r#1", CiStatus::None, ReviewStatus::None);
        ws.snoozed_until = Some(Utc::now() + chrono::Duration::hours(1));
        match resolve_short_snooze(Some(&ws), Utc::now(), Duration::from_secs(60)) {
            Intent::Unsnooze { .. } => {}
            other => panic!("expected Unsnooze, got {other:?}"),
        }
    }

    // ── Snooze (long) ────────────────────────────────────────────

    #[test]
    fn long_snooze_always_snoozes() {
        // Unlike short-snooze, the long-snooze does NOT toggle —
        // confirming x z snoozes
        // for another year. That's the model's contract; pin it.
        let mut ws = pr("o/r#1", CiStatus::None, ReviewStatus::None);
        ws.snoozed_until = Some(Utc::now() + chrono::Duration::hours(1));
        let d = Duration::from_secs(365 * 24 * 3600);
        match resolve_long_snooze(Some(&ws), d) {
            Intent::Snooze { duration, .. } => assert_eq!(duration, d),
            other => panic!("expected Snooze, got {other:?}"),
        }
    }

    // ── Mark read ────────────────────────────────────────────────

    #[test]
    fn mark_read_with_workspace_returns_mark_all_read() {
        let ws = pr("o/r#1", CiStatus::None, ReviewStatus::None);
        match resolve_mark_read(Some(&ws)) {
            Intent::MarkAllRead { session_key } => {
                assert_eq!(session_key.as_str(), ws.key.as_str());
            }
            other => panic!("expected MarkAllRead, got {other:?}"),
        }
    }

    #[test]
    fn agent_id_is_honored() {
        let ws = pr("o/r#1", CiStatus::Failure, ReviewStatus::Pending);
        let intent = resolve_work(
            Some(&ws),
            &[],
            "codex",
            &lazybox_core::Conventions::default(),
        );
        match intent {
            Intent::SpawnAgent { agent_id, .. } => assert_eq!(agent_id, "codex"),
            other => panic!("expected SpawnAgent, got {other:?}"),
        }
    }

    // ── resolve_spawn_shell ───────────────────────────────────────

    #[test]
    fn spawn_shell_no_workspace_is_noop() {
        // Match what the old handler did (drop the spawn silently),
        // but now it's typed + testable + the contextual footer can
        // hide `s` when no workspace is selected.
        assert_eq!(resolve_spawn_shell(None), Intent::NoOp);
    }

    #[test]
    fn spawn_shell_with_workspace_emits_spawn_shell() {
        let ws = pr("o/r#1", CiStatus::None, ReviewStatus::None);
        match resolve_spawn_shell(Some(&ws)) {
            Intent::SpawnShell { workspace_key } => {
                assert_eq!(workspace_key.as_str(), ws.key.as_str());
            }
            other => panic!("expected SpawnShell, got {other:?}"),
        }
    }

    // ── resolve_spawn_agent ───────────────────────────────────────

    #[test]
    fn spawn_agent_no_workspace_is_noop() {
        assert_eq!(resolve_spawn_agent(None, "claude"), Intent::NoOp);
    }

    #[test]
    fn spawn_agent_empty_id_is_noop() {
        // Defensive: a misconfigured shortcut (empty string in the
        // map) must NOT cause a bare-process spawn. Catch the typo
        // here rather than relying on the daemon to reject it.
        let ws = pr("o/r#1", CiStatus::None, ReviewStatus::None);
        assert_eq!(resolve_spawn_agent(Some(&ws), ""), Intent::NoOp);
    }

    #[test]
    fn spawn_agent_with_workspace_emits_spawn_agent_no_prompt() {
        let ws = pr("o/r#1", CiStatus::None, ReviewStatus::None);
        match resolve_spawn_agent(Some(&ws), "claude") {
            Intent::SpawnAgent {
                workspace_key,
                agent_id,
                prompt,
            } => {
                assert_eq!(workspace_key.as_str(), ws.key.as_str());
                assert_eq!(agent_id, "claude");
                assert!(prompt.is_none(), "bare spawn has no auto-prompt");
            }
            other => panic!("expected SpawnAgent, got {other:?}"),
        }
    }

    #[test]
    fn spawn_agent_id_passed_through_unchanged() {
        // Resolver is agnostic — the keymap decides which agent;
        // resolver just packages it into the Intent.
        let ws = pr("o/r#1", CiStatus::None, ReviewStatus::None);
        for id in ["claude", "codex", "cursor", "aider-custom"] {
            match resolve_spawn_agent(Some(&ws), id) {
                Intent::SpawnAgent { agent_id, .. } => assert_eq!(agent_id, id),
                other => panic!("expected SpawnAgent({id}), got {other:?}"),
            }
        }
    }

    #[test]
    fn mark_read_targets_prefers_selection_then_cursor_then_workspace() {
        let mut ws = pr("o/r#1", CiStatus::None, ReviewStatus::None);
        ws.activity.push(lazybox_core::Activity {
            author: "alice".into(),
            body: "first".into(),
            created_at: Utc::now(),
            kind: lazybox_core::ActivityKind::Comment,
            node_id: Some("node-1".into()),
            path: None,
            line: None,
            diff_hunk: None,
            thread_id: None,
        });

        assert!(matches!(
            resolve_mark_read_targets(Some(&ws), &[0], Some(0)),
            Intent::MarkActivitiesRead {
                targets,
                optimistic: false,
                notice: Some(_),
                ..
            } if targets.len() == 1
                && targets[0].fingerprint
                    == Some(ActivityFingerprint::NodeId("node-1".into()))
        ));
        assert!(matches!(
            resolve_mark_read_targets(Some(&ws), &[], Some(0)),
            Intent::MarkActivitiesRead {
                targets,
                optimistic: true,
                notice: None,
                ..
            } if targets.len() == 1
        ));
        assert!(matches!(
            resolve_mark_read_targets(Some(&ws), &[], None),
            Intent::MarkAllRead { .. }
        ));
    }

    #[test]
    fn collapse_into_pr_requires_a_claiming_pr() {
        let issue = issue("o/r#42");
        let mut claiming = pr("o/r#7", CiStatus::Success, ReviewStatus::Approved);
        claiming
            .pr
            .as_mut()
            .expect("pr")
            .closes_issues
            .push(issue.primary_task().expect("issue").id.clone());

        assert!(matches!(
            resolve_collapse_into_pr(Some(&issue), &[&issue, &claiming]),
            Intent::CollapseIntoPr { .. }
        ));
        assert!(matches!(
            resolve_collapse_into_pr(Some(&issue), &[&issue]),
            Intent::Notice(message) if message.contains("no PR closes")
        ));
    }

    #[test]
    fn send_to_session_distinguishes_missing_and_blank_captures() {
        let ws = pr("o/r#1", CiStatus::None, ReviewStatus::None);

        assert!(matches!(
            resolve_send_to_session(Some(&ws), None),
            Intent::Notice(message) if message.contains("no agent session")
        ));
        assert!(matches!(
            resolve_send_to_session(Some(&ws), Some(String::new())),
            Intent::MountHandoffPicker {
                seed,
                notice: Some(_),
                ..
            } if seed.is_empty()
        ));
    }

    #[test]
    fn pending_notice_names_the_pr_number() {
        let ws = pr("o/r#42", CiStatus::Success, ReviewStatus::Approved);
        let intent = resolve_merge(Some(&ws));

        assert_eq!(
            pending_notice(&intent, Some(&ws)).as_deref(),
            Some("merging PR #42…")
        );
    }
}
