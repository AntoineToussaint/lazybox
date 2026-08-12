//! Modal-mount helpers — every `mount_*` for Reply, NewWorkspace,
//! NewProject, RequestReviewers, AddAssignees, Help,
//! RemoveOutOfScope, ActionConfirm, CleanWorktrees, SidebarContext,
//! AdoptTarget, MergeConfirm — plus the candidate-login gatherer
//! used by the picker modals and the `push_modal` / `pop_modal`
//! z-stack helpers.
//!
//! Every method here owns one modal's mount path: build the
//! component, push onto `modal_stack`, mark it active, set
//! `redraw`. The matching `handle_input_submitted` /
//! `handle_choice_picked` / `handle_confirmed` arms (in mod.rs or
//! events.rs) read the stashed state and execute on submit.

use super::{ChoicePayload, ConversionDraft, HandoffDraft, Id, ModalFlow, Model};
use tuirealm::terminal::TerminalAdapter;

/// Choice-modal item wrapper for the worktree inspector. Picker
/// returns indices; we store one of these per row so the
/// `ChoicePicked` handler knows whether the user hit the bulk
/// shortcut or a specific worktree.
#[derive(Debug, Clone)]
pub(super) enum InspectRow {
    /// First slot in the list (when there is at least one safe-to-
    /// delete orphan). Triggers `bulk_delete_safe_inspected`.
    BulkSafe {
        count: usize,
    },
    Inspection(lazybox_ipc::WorktreeInspectionDto),
}

/// What picking one row of the automation-policies menu (`g p`, issue
/// #363) does. The `ChoicePicked` handler resolves the index into this
/// against the live workspace so the toggle reads current state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PolicyToggle {
    /// Flip the workspace's client-side merge-on-green arm.
    MergeOnGreen,
    /// Flip the per-session auto-fix arm for one failure kind.
    AutoFix(lazybox_core::AutoFixKind),
    /// A read-only / not-applicable row (native auto-merge status, or a
    /// PR-only policy shown on an issue). Selecting re-informs, no
    /// command.
    Info(String),
}

/// Stash for the `w` multi-agent chooser (`Id::WorkAgentPicker`,
/// #418): the exact running terminals shown (row order matches the
/// picker), plus the spawn params the pick replays through
/// `push_work_command`.
#[derive(Debug, Clone)]
pub(crate) struct PendingWorkPicker {
    pub targets: Vec<crate::components::sidebar::RunningWorkTarget>,
    pub session_id: Option<lazybox_core::SessionId>,
    pub model_alias: Option<String>,
}

/// Build the automation-policies menu rows for `ws`: one `(label,
/// toggle)` pair per policy, reflecting current state. `opt_out_labels`
/// is the configured auto-fix opt-out set so a label that disables
/// auto-fix is surfaced rather than invisible (issue #363 acceptance);
/// `auto_fix_enabled` is the global switch, so an armed workspace reads
/// as off while the feature is globally disabled — matching what would
/// actually fire.
pub(crate) fn build_policy_rows(
    ws: &lazybox_core::Workspace,
    auto_fix_enabled: bool,
    opt_out_labels: &[String],
) -> (Vec<String>, Vec<PolicyToggle>) {
    let mut labels = Vec::new();
    let mut toggles = Vec::new();
    let glyph = |on: bool| if on { "●" } else { "○" };

    match ws.pr.as_ref() {
        Some(pr) => {
            // 1. merge-on-green (client-side). Superseded by native
            //    auto-merge when that's on (precedence, issue #363). The
            //    detail spells out the durability difference the ` ARM `
            //    pill can't (#794): lazybox merges it, but only while
            //    lazybox is running.
            let on = ws.auto_merge_on_green;
            let detail = if pr.auto_merge_enabled {
                "  (lazybox · GitHub auto-merge takes over)"
            } else {
                "  (lazybox · merges only while lazybox runs)"
            };
            labels.push(format!("{} merge on green{detail}", glyph(on)));
            toggles.push(PolicyToggle::MergeOnGreen);

            // 2. GitHub-native auto-merge — read-only status. Named as the
            //    durable counterpart (#794): GitHub lands the PR server-side,
            //    so it works even with lazybox closed.
            labels.push(format!(
                "{} GitHub auto-merge  (GitHub · merges even when lazybox is closed)",
                glyph(pr.auto_merge_enabled)
            ));
            toggles.push(PolicyToggle::Info(
                "GitHub-native auto-merge is set on github.com, not in lazybox — it merges \
                 server-side even while lazybox is closed"
                    .into(),
            ));

            // 3 + 4. per-session auto-fix arms.
            for kind in [
                lazybox_core::AutoFixKind::CiFailure,
                lazybox_core::AutoFixKind::MergeConflict,
            ] {
                let opted_out = auto_fix_label_opt_out(pr, opt_out_labels);
                let arm = ws.policies.arm(kind);
                // Gate through the same core composition the daemon uses
                // (enable + label + arm), so the glyph can't drift from
                // what would actually fire (tracker #512).
                let on =
                    lazybox_core::auto_fix_enabled_and_permitted(auto_fix_enabled, opted_out, arm);
                let name = match kind {
                    lazybox_core::AutoFixKind::CiFailure => "auto-fix CI",
                    lazybox_core::AutoFixKind::MergeConflict => "auto-fix conflict",
                };
                let detail = match arm {
                    // A globally-disabled feature never fires, whatever the
                    // arm — say so rather than claim an armed row is on.
                    _ if !auto_fix_enabled => "  (off · auto-fix disabled globally)".to_string(),
                    lazybox_core::PolicyArm::Disarm => "  (disarmed here)".to_string(),
                    lazybox_core::PolicyArm::Arm => "  (armed here · overrides label)".to_string(),
                    lazybox_core::PolicyArm::Default if opted_out => {
                        "  (off · opt-out label)".to_string()
                    }
                    lazybox_core::PolicyArm::Default => "  (follows config)".to_string(),
                };
                labels.push(format!("{} {name}{detail}", glyph(on)));
                toggles.push(PolicyToggle::AutoFix(kind));
            }
        }
        None => {
            // Issue-only workspace: every policy here targets a PR.
            // Surface them as unavailable so "which apply to issues vs
            // PRs" is explicit rather than a silent absence.
            for name in ["merge on green", "auto-fix CI", "auto-fix conflict"] {
                labels.push(format!("○ {name}  (PR only)"));
                toggles.push(PolicyToggle::Info(format!("{name} applies to a PR")));
            }
        }
    }
    (labels, toggles)
}

/// Whether any configured opt-out label is present on the PR
/// (case-insensitive). Mirrors `lazybox_core::is_auto_fix_opted_out`
/// but on the client's display-only label set.
fn auto_fix_label_opt_out(pr: &lazybox_core::Task, opt_out_labels: &[String]) -> bool {
    pr.labels.iter().any(|label| {
        opt_out_labels
            .iter()
            .any(|opt| opt.eq_ignore_ascii_case(&label.name))
    })
}

/// Tabular label for one inspector row. Single-line — the Choice
/// modal truncates with an ellipsis when it overflows. Pack the
/// signal-dense bits first: name, reasons, size, age, flags.
/// One row of the import picker: `owner/repo · /path · branch [DIRTY]`,
/// falling back to the path when the checkout has no GitHub origin.
fn format_discovered_checkout(c: &lazybox_ipc::DiscoveredCheckoutDto) -> String {
    let repo = c.repo.as_deref().unwrap_or("(no origin)");
    let branch = c.branch.as_deref().unwrap_or("(detached)");
    let dirty = if c.has_uncommitted_changes {
        " [DIRTY]"
    } else {
        ""
    };
    format!("{repo} · {} · {branch}{dirty}", c.path.display())
}

fn format_inspect_row(row: &InspectRow) -> String {
    match row {
        InspectRow::BulkSafe { count } => {
            format!("▶ Delete all {count} clearly-safe worktrees")
        }
        InspectRow::Inspection(dto) => {
            let name = dto
                .path
                .file_name()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_else(|| dto.path.to_string_lossy().into_owned());
            let reasons = if dto.reasons.is_empty() {
                "ok".to_string()
            } else {
                dto.reasons.join(",")
            };
            let mut flags = Vec::<&str>::new();
            if dto.has_uncommitted_changes {
                flags.push("DIRTY");
            }
            if dto.has_unpushed_commits {
                flags.push("UNPUSHED");
            }
            let flag_str = if flags.is_empty() {
                String::new()
            } else {
                format!(" [{}]", flags.join(","))
            };
            let branch = dto.branch.as_deref().unwrap_or("(detached)");
            let size = format_size(dto.size_bytes);
            let age = format_age_short(dto.last_modified_unix);
            format!("[{reasons}] {name} · {branch} · {size} · {age}{flag_str}")
        }
    }
}

fn format_size(n: u64) -> String {
    const KB: u64 = 1024;
    const MB: u64 = 1024 * KB;
    const GB: u64 = 1024 * MB;
    if n >= GB {
        format!("{:.1}G", n as f64 / GB as f64)
    } else if n >= MB {
        format!("{:.1}M", n as f64 / MB as f64)
    } else if n >= KB {
        format!("{:.1}K", n as f64 / KB as f64)
    } else {
        format!("{n}B")
    }
}

/// "N running terminal(s)" with correct pluralization.
fn terminals_phrase(count: usize) -> String {
    if count == 1 {
        "1 running terminal".to_string()
    } else {
        format!("{count} running terminals")
    }
}

/// Confirm copy for folding an issue with live terminals into the PR
/// that closes it. Says "join" — matching the `x j` "join issue
/// into PR" action and the follow-up flash — rather than "merge", which
/// would read like the nearby `g m` git-merge action (issue #314).
fn merge_prompt_question(pr_label: &str, issue_label: &str, count: usize) -> String {
    format!(
        "{pr_label} closes {issue_label}, which has {phrase}. \
         Join the issue's sessions into the PR workspace?",
        phrase = terminals_phrase(count),
    )
}

/// Confirm copy for an out-of-scope workspace with live terminals.
/// Trims the title so a verbose PR description doesn't make the modal
/// three lines tall — 80 chars + an ellipsis fits the dynamic-height
/// Confirm cleanly.
fn out_of_scope_copy(prompt: &super::RemovalPrompt) -> String {
    let phrase = terminals_phrase(prompt.terminal_count);
    let label = &prompt.label;
    match prompt.title.as_deref().filter(|s| !s.is_empty()) {
        Some(t) => {
            let title_short = if t.chars().count() > 80 {
                let truncated: String = t.chars().take(79).collect();
                format!("{truncated}…")
            } else {
                t.to_string()
            };
            format!(
                "{label} \"{title_short}\" is no longer in your filter scope but has {phrase} — kill and remove?"
            )
        }
        None => {
            format!("{label} is no longer in your filter scope but has {phrase} — kill and remove?")
        }
    }
}

/// Confirm copy for a workspace whose task reached a terminal state —
/// `verb` is "merged" (PR) or "closed" (issue). Always names the
/// worktree deletion; appends a warning when there are live terminals
/// or local (uncommitted/unpushed) work, since "yes" force-deletes
/// regardless.
fn terminal_removal_copy(prompt: &super::RemovalPrompt, verb: &str) -> String {
    let label = &prompt.label;
    let mut warnings: Vec<String> = Vec::new();
    if prompt.terminal_count > 0 {
        warnings.push(terminals_phrase(prompt.terminal_count));
    }
    if prompt.has_local_work {
        warnings.push("uncommitted or unpushed work".to_string());
    }
    if warnings.is_empty() {
        format!("{label} was {verb} — remove workspace and delete its worktree?")
    } else {
        format!(
            "{label} was {verb} — remove workspace and delete its worktree? \
             Warning: it has {} that will be lost.",
            warnings.join(" and "),
        )
    }
}

/// Trim a snippet body for the confirm preview: keep the first 12
/// lines, marking with an ellipsis when more was dropped, so a long
/// body can't push the Y/N buttons past the modal.
/// The confirm-preview text for a `scaffold_skill` proposal (#799).
/// Names the destination *and* how it was resolved (`root.describe()`),
/// so a launch-directory fallback reads plainly and is never mistaken
/// for the focused workspace's own repo. `name`/`description` are
/// expected pre-trimmed by the caller.
pub(super) fn skill_scaffold_preview(
    root: &super::inputs::SkillScaffoldRoot,
    name: &str,
    description: &str,
    body: &str,
) -> String {
    let path = lazybox_config::skill_md_path(root.path(), name);
    let mut preview = format!(
        "Scaffold skill `{name}` in {} —\n{}?\n\n",
        root.describe(),
        path.display(),
    );
    preview.push_str(&format!("description: {description}\n\n"));
    preview.push_str(&snippet_body_preview(body));
    preview.push_str("\n\nWrites a new SKILL.md the focused agent can load on its own.");
    preview
}

fn snippet_body_preview(body: &str) -> String {
    const MAX_LINES: usize = 12;
    let lines: Vec<&str> = body.lines().collect();
    if lines.len() <= MAX_LINES {
        return body.trim_end().to_string();
    }
    let mut out = lines[..MAX_LINES].join("\n");
    out.push_str("\n…");
    out
}

/// Human-friendly workspace label for the autonomous-spawn footer
/// notice: drop the `source:` prefix so a key like
/// `github:owner/repo#7` reads as `owner/repo#7`. Keys without a prefix
/// (local projects) pass through unchanged.
fn worktree_notice_label(session_key: &lazybox_core::SessionKey) -> String {
    session_key
        .as_str()
        .split_once(':')
        .map(|(_, rest)| rest)
        .unwrap_or_else(|| session_key.as_str())
        .to_string()
}

fn format_age_short(unix_secs: Option<u64>) -> String {
    let Some(t) = unix_secs else {
        return "—".into();
    };
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(t);
    let secs = now.saturating_sub(t);
    if secs < 60 {
        format!("{secs}s")
    } else if secs < 3600 {
        format!("{}m", secs / 60)
    } else if secs < 86_400 {
        format!("{}h", secs / 3600)
    } else {
        format!("{}d", secs / 86_400)
    }
}

impl<T: TerminalAdapter> Model<T> {
    /// Mount the reply textarea targeted at `workspace_key`. Submit
    /// → `Msg::TextareaSubmitted(body)` → orchestrator builds a
    /// `Command::PostReply { session_key, body }`.
    pub(super) fn mount_reply(&mut self, workspace_key: lazybox_core::SessionKey) {
        use crate::realm::components::textarea::Textarea;

        if matches!(self.modal_stack.last(), Some(Id::Reply)) {
            return;
        }

        let label = workspace_key.to_string();
        let modal = Textarea::new("Reply").with_header(format!("on {label}"));
        self.set_modal_flow(ModalFlow::Reply {
            target: workspace_key,
        });
        self.mount_modal(Id::Reply, modal);
    }

    /// Mount the notes editor for `workspace_key`, pre-filled with the
    /// workspace's current local note (issue #458). Submit →
    /// `Msg::TextareaSubmitted(body)` → orchestrator builds a
    /// `Command::SetNotes { session_key, notes }`.
    pub(super) fn mount_notes(&mut self, workspace_key: lazybox_core::SessionKey) {
        use crate::realm::components::textarea::Textarea;

        if matches!(self.modal_stack.last(), Some(Id::Notes)) {
            return;
        }

        let existing = self
            .sidebar
            .workspace_by_key(&workspace_key)
            .map(|w| w.notes.clone())
            .unwrap_or_default();
        let label = workspace_key.to_string();
        let modal = Textarea::new("Notes")
            .with_header(format!("local scratchpad — {label} (never synced)"))
            .with_body(existing)
            .allow_empty();
        self.set_modal_flow(ModalFlow::Notes {
            target: workspace_key,
        });
        self.mount_modal(Id::Notes, modal);
    }

    /// Mount the "New workspace" name prompt under a specific
    /// Project. Submit → `Msg::InputSubmitted(name)` while
    /// `Id::NewWorkspace` is on top → `Command::CreateWorkspace
    /// { name, project_key }`. The project_key is stashed on self
    /// here and consumed by `handle_input_submitted`.
    pub(super) fn mount_new_workspace_input(&mut self, project_key: lazybox_core::ProjectKey) {
        use crate::realm::components::input::Input;

        // Don't preempt an open modal. The `x p` / start-agent pickers
        // pop themselves before reaching here (empty stack), but the
        // async `ProjectUpserted` hand-off can race a modal the user
        // opened in the meantime — mounting over it would arm a second
        // `modal_flow` on top of that modal's live continuation
        // (clobbering it, or tripping `set_modal_flow`'s debug assert).
        // Mirrors the label / inspector "wait for an empty stack"
        // convention; the project header is already focused, so the user
        // can still press `x n`. Subsumes the old NewWorkspace-only
        // idempotence check.
        if !self.modal_stack.is_empty() {
            return;
        }
        self.set_modal_flow(ModalFlow::NewWorkspaceProject {
            project: project_key,
        });

        let modal = Input::new("Name this workspace")
            .title("New workspace")
            .placeholder("e.g. spike-rate-limit, refactor-auth, …")
            .with_validator(|s: &str| !s.trim().is_empty());
        self.mount_modal(Id::NewWorkspace, modal);
    }

    /// Mount the "Rename workspace" prompt for the focused workspace,
    /// prefilled with its current display name (issue #744). Submit →
    /// `Msg::InputSubmitted(name)` while `Id::RenameWorkspace` is on top
    /// → `Command::RenameWorkspace { session_key, name }`. The target key
    /// is stashed on the `ModalFlow::RenameWorkspace` flow and consumed by
    /// `handle_input_submitted`.
    pub(super) fn mount_rename_workspace_input(&mut self, workspace_key: lazybox_core::SessionKey) {
        use crate::realm::components::input::Input;

        if matches!(self.modal_stack.last(), Some(Id::RenameWorkspace)) {
            return;
        }

        let current = self
            .sidebar
            .workspace_by_key(&workspace_key)
            .map(|w| w.name.clone())
            .unwrap_or_default();
        self.set_modal_flow(ModalFlow::RenameWorkspace {
            target: workspace_key,
        });

        let modal = Input::new("Rename this workspace")
            .title("Rename workspace")
            .with_input(current)
            .with_validator(|s: &str| !s.trim().is_empty());
        self.mount_modal(Id::RenameWorkspace, modal);
    }

    /// Mount the "Move to Space" name prompt (#860). Submit →
    /// `Msg::InputSubmitted(space)` while `Id::MoveToSpace` is on top →
    /// `Sidebar::assign_source_to_space`. The source label is
    /// stashed on the `ModalFlow::MoveToSpace` flow; the input is
    /// prefilled with the source's current Space so a bare Enter is a
    /// no-op and clearing it unassigns.
    pub(super) fn mount_move_to_space_input(&mut self, source: String) {
        use crate::realm::components::input::Input;

        if matches!(self.modal_stack.last(), Some(Id::MoveToSpace)) {
            return;
        }

        let current = self.sidebar.space_of_source(&source);
        self.set_modal_flow(ModalFlow::MoveToSpace {
            source: source.clone(),
        });

        let modal = Input::new(format!("Move {source} to which Space? (blank = default)"))
            .title("Move to Space")
            .with_input(current);
        self.mount_modal(Id::MoveToSpace, modal);
    }

    /// Mount the "New project" name prompt. Submit →
    /// `Msg::InputSubmitted(name)` while `Id::NewProject` is on top
    /// → `Command::CreateProject { name }`. Daemon creates a local
    /// project keyed `local-<slug>` (idempotent on collision).
    pub(super) fn mount_new_project_input(&mut self) {
        use crate::realm::components::input::Input;

        if matches!(self.modal_stack.last(), Some(Id::NewProject)) {
            return;
        }

        let modal = Input::new("Name this project")
            .title("New project")
            .placeholder("e.g. my-experiments, side-quests, scratch, …")
            .with_validator(|s: &str| !s.trim().is_empty());
        self.mount_modal(Id::NewProject, modal);
    }

    /// Drive the `x p` new-workspace flow: pick a tracked repo to
    /// spin a workspace up on, with "create a new local project" kept
    /// as an explicit escape hatch rather than the forced first step.
    ///
    /// - **No tracked repos** → there's nothing to pick, so go
    ///   straight to the new-project input (the only way to bootstrap
    ///   a brand-new user with an empty inbox).
    /// - **One or more** → mount a picker listing each repo plus a
    ///   trailing "new local project" row. The pick funnels into the
    ///   new-workspace name input under the chosen repo
    ///   (`handle_choice_picked`), or into the new-project input.
    pub(crate) fn mount_new_workspace_repo_picker(&mut self) {
        use crate::realm::components::choice::Choice;

        if matches!(self.modal_stack.last(), Some(Id::NewWorkspaceRepo)) {
            return;
        }
        let projects = self.sidebar.projects_for_picker();
        if projects.is_empty() {
            self.mount_new_project_input();
            return;
        }
        // Each row carries its project key (or `None` for the trailing
        // "new local project" escape hatch) so the pick resolves to the
        // right repo regardless of row order (#512).
        type RepoRow = (String, Option<lazybox_core::ProjectKey>);
        let mut items: Vec<RepoRow> = projects
            .into_iter()
            .map(|(k, name)| (name, Some(k)))
            .collect();
        items.push(("＋ Create a new local project…".to_string(), None));

        let modal = Choice::single("Start a workspace on which repo?", items)
            .title("New workspace")
            .label(|(name, _): &RepoRow| name.clone())
            .payload_for(|(_, key): &RepoRow| match key {
                Some(k) => ChoicePayload::Project(k.clone()),
                None => ChoicePayload::NewLocalProject,
            });
        self.mount_modal(Id::NewWorkspaceRepo, modal);
    }

    /// Mount the "request reviewers" multi-select picker for the
    /// given workspace's PR. Candidates are gathered from the
    /// workspace's known people; Space toggles, Enter submits →
    /// `Msg::ChoicePicked` — each row carrying its login as a
    /// [`ChoicePayload::Text`] — → `handle_choice_picked` collects the
    /// chosen logins and dispatches `Command::RequestReviewers`.
    pub(crate) fn mount_request_reviewers(&mut self, workspace_key: lazybox_core::WorkspaceKey) {
        use crate::realm::components::choice::Choice;

        if matches!(self.modal_stack.last(), Some(Id::RequestReviewers)) {
            return;
        }
        let candidates = self.gather_candidate_logins(&workspace_key, true);
        // With no candidates, mount the picker with an explanatory
        // empty state rather than only flashing — a bare footer flash
        // is easy to miss, and the framed notice reads clearly over
        // the panes. `Choice` sizes itself down when its list is
        // empty so this never renders as a blank rectangle (#35).
        let modal = if candidates.is_empty() {
            Choice::<String>::multi(
                "No candidate reviewers yet.\n\nInteract with the PR — comment, review, or assign\nsomeone — and they'll show up here to pick from.",
                Vec::new(),
            )
            .title("Add reviewers")
            .label(|s: &String| s.clone())
        } else {
            // Items are the bare logins; the `@` prefix is display-only
            // and the payload carries the login itself (#512).
            Choice::multi("Request review from", candidates)
                .title("Add reviewers")
                .label(|l: &String| format!("@{l}"))
                .payload_for(|l: &String| ChoicePayload::Text(l.clone()))
        };
        self.set_modal_flow(ModalFlow::ReviewRequest {
            workspace: workspace_key,
        });
        self.mount_modal(Id::RequestReviewers, modal);
    }

    /// Mount the unified automation-policies menu (`g p`, issue #363)
    /// for `workspace_key`. Single-pick `Choice` whose rows are every
    /// policy on the focused PR/issue with its on/off state; picking a
    /// row dispatches its toggle (see `choice_picked_inner`). Each row
    /// carries its toggle as a [`ChoicePayload::Policy`], which the
    /// handler resolves back.
    pub(crate) fn mount_policy_picker(&mut self, workspace_key: lazybox_core::WorkspaceKey) {
        use crate::realm::components::choice::Choice;

        if matches!(self.modal_stack.last(), Some(Id::PolicyPicker)) {
            return;
        }
        let Some(ws) = self
            .sidebar
            .workspace_iter()
            .find(|(k, _)| k.as_str() == workspace_key.as_str())
            .map(|(_, w)| w)
        else {
            return;
        };
        let (labels, toggles) =
            build_policy_rows(ws, self.auto_fix_enabled, &self.auto_fix_opt_out_labels);
        // Pair each label with the toggle it fires so the pick carries
        // its own `PolicyToggle` (#512).
        let items: Vec<(String, PolicyToggle)> = labels.into_iter().zip(toggles).collect();
        self.set_modal_flow(ModalFlow::PolicyWorkspace {
            workspace: workspace_key,
        });
        let modal = Choice::single("● armed · ○ off — Enter toggles", items)
            .title("Automation policies")
            .label(|(l, _): &(String, PolicyToggle)| l.clone())
            .payload_for(|(_, t): &(String, PolicyToggle)| ChoicePayload::Policy(t.clone()));
        self.mount_modal(Id::PolicyPicker, modal);
    }

    /// Mount the composable filter menu (`f`, `OpenFilterMenu`). A
    /// multi-select `Choice` over every filter, grouped by axis
    /// (State / Role / Kind), each row carrying its match count and
    /// pre-checked when already active. Space toggles, Enter replaces
    /// the sidebar's active set (an empty submit clears all filters).
    pub(crate) fn mount_filter_menu(&mut self) {
        use crate::components::sidebar::FilterEntry;
        use crate::realm::components::choice::Choice;
        use std::collections::HashMap;

        if matches!(self.modal_stack.last(), Some(Id::FilterMenu)) {
            return;
        }
        // Fixed predicates + the label / Linear-state values present in
        // the current mailbox, each with its match count.
        let entries = self.sidebar.filter_menu_entries();
        let counts: HashMap<FilterEntry, usize> = entries.iter().cloned().collect();
        let items: Vec<FilterEntry> = entries.into_iter().map(|(e, _)| e).collect();
        let active = self.sidebar.filters().clone();
        let modal = Choice::multi(
            "Space toggles · Enter applies · same section = any (OR), across sections = all (AND)",
            items,
        )
        .title("Filters")
        .section_for(|e: &FilterEntry| e.axis().label())
        .label(move |e: &FilterEntry| {
            format!("{} ({})", e.label(), counts.get(e).copied().unwrap_or(0))
        })
        // Each row carries its own entry, so the grouped display can't
        // resolve to the wrong predicate (#512).
        .payload_for(|e: &FilterEntry| ChoicePayload::Filter(e.clone()))
        .with_selected_by(move |e: &FilterEntry| active.contains_entry(e))
        .allow_empty(true);
        self.mount_modal(Id::FilterMenu, modal);
    }

    /// Mount the `w` multi-conversation chooser (#418): the selected
    /// workspace has several running agent terminals, so ask which exact
    /// one to inject into instead of silently guessing. `session_id` /
    /// `model_alias` carry the original `w` / `w S` parameters through.
    pub(crate) fn mount_work_agent_picker(
        &mut self,
        targets: Vec<crate::components::sidebar::RunningWorkTarget>,
        session_id: Option<lazybox_core::SessionId>,
        model_alias: Option<String>,
    ) {
        use crate::realm::components::choice::Choice;

        if matches!(self.modal_stack.last(), Some(Id::WorkAgentPicker)) {
            return;
        }
        // `w` would have nothing to do after the pick — say so now
        // instead of asking a question whose answer goes nowhere.
        let workspace = self.sidebar.selected_workspace();
        let selected = self.right.selected_activity_indices();
        if crate::intent::classify_work(workspace, &selected).is_none() {
            self.flash_hint("nothing to work on here");
            return;
        }
        let labels: Vec<String> = targets
            .iter()
            .map(|target| {
                let duplicate_count = targets
                    .iter()
                    .filter(|other| other.agent_id == target.agent_id)
                    .count();
                if duplicate_count > 1 {
                    format!("{} · terminal {}", target.agent_id, target.terminal_id.0)
                } else {
                    target.agent_id.clone()
                }
            })
            .collect();
        self.set_modal_flow(ModalFlow::WorkPicker {
            picker: PendingWorkPicker {
                targets,
                session_id,
                model_alias,
            },
        });
        let modal = Choice::single("Several conversations are running — inject into…", labels)
            .title("Work with which conversation?")
            .label(|label: &String| label.clone());
        self.mount_modal(Id::WorkAgentPicker, modal);
    }

    /// Mount the "assignees" multi-select picker for the workspace's
    /// PR or issue. Pre-checks the currently-assigned logins so this
    /// is a "change assignees" UX (toggle to add / untoggle to
    /// remove) rather than an additive picker — submitting fires
    /// `Command::SetAssignees`, which diffs against the persisted
    /// task and runs add + remove mutations as needed.
    pub(crate) fn mount_add_assignees(&mut self, workspace_key: lazybox_core::WorkspaceKey) {
        use crate::realm::components::choice::Choice;

        if matches!(self.modal_stack.last(), Some(Id::AddAssignees)) {
            return;
        }
        // Include existing assignees in the candidate list (they're
        // pre-checked below) so the user can untick to remove. The
        // old shape filtered them out, making the picker add-only.
        let candidates = self.gather_candidate_logins_inclusive(&workspace_key);
        if candidates.is_empty() {
            self.flash_info("no candidate assignees yet — interact with the task first");
            return;
        }
        // Pre-tick the currently-assigned logins. `with_selected_by`
        // walks the items and sets the selected bit for any match.
        let existing: std::collections::HashSet<String> = self
            .sidebar
            .workspace_iter()
            .find(|(k, _)| k.as_str() == workspace_key.as_str())
            .and_then(|(_, w)| w.primary_task().map(|t| t.assignees.clone()))
            .unwrap_or_default()
            .into_iter()
            .collect();
        self.set_modal_flow(ModalFlow::AssigneesRequest {
            workspace: workspace_key,
        });
        // Items are bare logins; `@` is display-only, the payload is the
        // login (#512).
        let modal = Choice::multi("Assign to", candidates)
            .title("Assignees (toggle to add/remove)")
            .label(|l: &String| format!("@{l}"))
            .payload_for(|l: &String| ChoicePayload::Text(l.clone()))
            .with_selected_by(move |login: &String| existing.contains(login));
        self.mount_modal(Id::AddAssignees, modal);
    }

    /// Mount the label picker once the daemon has answered our
    /// `FetchRepoLabels` request. Called from `handle_repo_labels`
    /// when the event lands for the workspace this picker is waiting
    /// on. Pre-checks the labels already applied to the task so the
    /// picker reads as "change the label set" rather than "add
    /// labels"; submit replaces the upstream set via `SetLabels`.
    pub(crate) fn mount_manage_labels(
        &mut self,
        workspace_key: lazybox_core::WorkspaceKey,
        repo_labels: Vec<lazybox_core::Label>,
    ) {
        use crate::realm::components::choice::Choice;

        // Async mount — the `RepoLabels` reply can arrive seconds after
        // the `g l` press, by which time the user may have opened
        // something else (a Reply textarea, a confirm). Mounting on top
        // would steal keyboard focus mid-typing, so this follows the
        // queued daemon prompts' wait-for-empty-stack rule: any modal
        // already up wins, and the fetch result is dropped (the user
        // can press `g l` again — the hint says so). Also covers the
        // old ManageLabels-only idempotence check.
        if let Some(top) = self.modal_stack.last() {
            tracing::info!(?top, "label picker skipped — another modal owns the stack");
            // Disarm the request stash so a later stray `RepoLabels`
            // broadcast can't mount the picker unprompted.
            self.awaiting_repo_labels = None;
            if !matches!(top, Id::ManageLabels) {
                self.flash_hint("labels loaded, but another dialog was open — press g l again");
            }
            return;
        }
        if repo_labels.is_empty() {
            self.flash_info("no labels defined on this repo");
            return;
        }
        // Pre-tick the labels currently applied to the task. Union
        // across PR + first issue so a workspace that has both still
        // sees its task-side labels checked.
        let existing: std::collections::HashSet<String> = self
            .sidebar
            .workspace_iter()
            .find(|(k, _)| k.as_str() == workspace_key.as_str())
            .map(|(_, w)| {
                let mut set: std::collections::HashSet<String> = std::collections::HashSet::new();
                if let Some(pr) = &w.pr {
                    for l in &pr.labels {
                        set.insert(l.name.clone());
                    }
                }
                if let Some(issue) = w.gh_issues.first() {
                    for l in &issue.labels {
                        set.insert(l.name.clone());
                    }
                }
                set
            })
            .unwrap_or_default();
        // Items are the bare label names (what the picker submits back
        // upstream); each row renders `[name]` to match the sidebar's
        // chip framing, and carries its bare name as the payload (#512).
        let names: Vec<String> = repo_labels.into_iter().map(|l| l.name).collect();
        self.awaiting_repo_labels = Some(workspace_key);
        let modal = Choice::multi("Apply labels", names)
            .title("Labels (toggle to add/remove)")
            .label(|name: &String| format!("[{name}]"))
            .payload_for(|name: &String| ChoicePayload::Text(name.clone()))
            .with_selected_by(move |name: &String| existing.contains(name));
        self.mount_modal(Id::ManageLabels, modal);
    }

    /// Mount the snooze duration picker. Used by `z` (ToggleSnooze)
    /// when the workspace is NOT currently snoozed — the user picks
    /// the duration instead of always paying the YAML default.
    /// Cycle of options is curated: each one's a "I'll come back
    /// to this when…" moment that maps to a real schedule.
    pub(crate) fn mount_snooze_picker(&mut self, session_key: lazybox_core::SessionKey) {
        use crate::realm::components::choice::Choice;
        use chrono::Datelike;
        use std::time::Duration;
        if matches!(self.modal_stack.last(), Some(Id::SnoozeDuration)) {
            return;
        }
        // Anchor "tomorrow / next week" on the user's local wall
        // clock so "tomorrow morning" lands at ~9am local time, not
        // 9am UTC. Each option is computed as a `Duration` from
        // `now` so the daemon-side snooze deadline (which uses
        // UTC) doesn't need to know about the user's timezone.
        let now_local = chrono::Local::now();
        let now_naive = now_local.naive_local();

        let until_eod = {
            let today_6pm = now_local
                .date_naive()
                .and_hms_opt(18, 0, 0)
                .unwrap_or(now_naive);
            let diff = today_6pm.signed_duration_since(now_naive);
            // Clamp: a 17:55 press should snooze at least 1h, not 5min.
            diff.to_std()
                .unwrap_or(Duration::from_secs(3600))
                .max(Duration::from_secs(3600))
        };
        let until_tomorrow = {
            let tomorrow_9am = (now_local + chrono::Duration::days(1))
                .date_naive()
                .and_hms_opt(9, 0, 0)
                .unwrap_or(now_naive);
            tomorrow_9am
                .signed_duration_since(now_naive)
                .to_std()
                .unwrap_or(Duration::from_secs(24 * 3600))
        };
        let until_next_week = {
            let weekday = now_local.weekday().num_days_from_monday() as i64;
            let days_until_monday = if weekday == 0 { 7 } else { 7 - weekday };
            let next_monday_9am = (now_local + chrono::Duration::days(days_until_monday))
                .date_naive()
                .and_hms_opt(9, 0, 0)
                .unwrap_or(now_naive);
            next_monday_9am
                .signed_duration_since(now_naive)
                .to_std()
                .unwrap_or(Duration::from_secs(7 * 24 * 3600))
        };
        let options: Vec<(&'static str, Duration)> = vec![
            ("1 hour", Duration::from_secs(3600)),
            ("4 hours (default)", Duration::from_secs(4 * 3600)),
            ("Until end of day (6pm)", until_eod),
            ("Tomorrow morning (9am)", until_tomorrow),
            ("Next Monday 9am", until_next_week),
            ("1 week", Duration::from_secs(7 * 24 * 3600)),
            ("1 month", Duration::from_secs(30 * 24 * 3600)),
            ("Forever (1 year)", Duration::from_secs(365 * 24 * 3600)),
        ];
        self.set_modal_flow(ModalFlow::Snooze {
            workspace: session_key,
        });
        // Each row carries its own duration (#512).
        let modal = Choice::single("Snooze for…", options)
            .title("Snooze duration")
            .label(|(l, _): &(&'static str, Duration)| (*l).to_string())
            .payload_for(|(_, d): &(&'static str, Duration)| ChoicePayload::Duration(*d));
        self.mount_modal(Id::SnoozeDuration, modal);
    }

    /// Mount the single global LLM-gateway URL input (Settings →
    /// "Configure LLM gateway"), pre-filled with the current value.
    /// Submit → `handle_input_submitted` writes `agent.llm_gateway_url`
    /// to YAML; an empty submission clears it.
    pub(crate) fn mount_gateway_url_input(&mut self) {
        use crate::realm::components::input::Input;

        if matches!(self.modal_stack.last(), Some(Id::LlmGatewayUrl)) {
            return;
        }
        let cfg = lazybox_config::Config::load().unwrap_or_default();
        let current = cfg.agent.gateway_url().unwrap_or_default().to_string();
        let modal = Input::new("LLM gateway base URL")
            .title("LLM gateway")
            .placeholder("e.g. http://gateway.internal (empty to disable)")
            .with_input(current);
        self.mount_modal(Id::LlmGatewayUrl, modal);
    }

    /// Build the candidate-logins list for the picker. Source set
    /// is the workspace's known people: existing reviewers,
    /// assignees, activity authors. Excludes the local user
    /// (no self-review) and either the existing reviewers (when
    /// building for the reviewer picker — they're already on the
    /// PR) OR the existing assignees (for the assignees picker).
    /// Dedupes; first-seen order preserved so the most relevant
    /// faces are at the top.
    /// Variant of `gather_candidate_logins` that *includes* the
    /// currently-assigned logins. Used by the assignees picker
    /// (which needs them visible + pre-checked) so the same submit
    /// path can untick → remove.
    pub(super) fn gather_candidate_logins_inclusive(
        &self,
        workspace_key: &lazybox_core::WorkspaceKey,
    ) -> Vec<String> {
        let Some(ws) = self
            .sidebar
            .workspace_iter()
            .find(|(k, _)| k.as_str() == workspace_key.as_str())
            .map(|(_, w)| w)
        else {
            return Vec::new();
        };
        let excluded: std::collections::HashSet<String> =
            self.viewer_logins.values().cloned().collect();
        let mut out: Vec<String> = Vec::new();
        let push = |login: &str, out: &mut Vec<String>| {
            if !login.is_empty() && !excluded.contains(login) && !out.iter().any(|l| l == login) {
                out.push(login.to_string());
            }
        };
        // Existing assignees go FIRST so the most relevant set
        // bubbles to the top of the list (and is naturally aligned
        // with the pre-checked items).
        if let Some(pr) = &ws.pr {
            for a in &pr.assignees {
                push(a, &mut out);
            }
            for r in &pr.reviewers {
                push(r, &mut out);
            }
        }
        for issue in &ws.gh_issues {
            for a in &issue.assignees {
                push(a, &mut out);
            }
        }
        for act in &ws.activity {
            push(&act.author, &mut out);
        }
        out
    }

    pub(super) fn gather_candidate_logins(
        &self,
        workspace_key: &lazybox_core::WorkspaceKey,
        exclude_existing_reviewers: bool,
    ) -> Vec<String> {
        let Some(ws) = self
            .sidebar
            .workspace_iter()
            .find(|(k, _)| k.as_str() == workspace_key.as_str())
            .map(|(_, w)| w)
        else {
            return Vec::new();
        };
        let mut excluded: std::collections::HashSet<String> =
            self.viewer_logins.values().cloned().collect();
        if exclude_existing_reviewers {
            if let Some(pr) = &ws.pr {
                for r in &pr.reviewers {
                    excluded.insert(r.clone());
                }
            }
        } else if let Some(pr) = &ws.pr {
            for a in &pr.assignees {
                excluded.insert(a.clone());
            }
        }
        let mut out: Vec<String> = Vec::new();
        let push = |login: &str, out: &mut Vec<String>| {
            if !login.is_empty() && !excluded.contains(login) && !out.iter().any(|l| l == login) {
                out.push(login.to_string());
            }
        };
        if let Some(pr) = &ws.pr {
            for a in &pr.assignees {
                push(a, &mut out);
            }
            for r in &pr.reviewers {
                push(r, &mut out);
            }
        }
        for issue in &ws.gh_issues {
            for a in &issue.assignees {
                push(a, &mut out);
            }
        }
        for act in &ws.activity {
            push(&act.author, &mut out);
        }
        out
    }

    /// Build + mount a Help modal listing the focused pane's keymap
    /// plus the global section. Idempotent: re-pressing `?` while
    /// help is up is a no-op (the existing modal stays).
    pub(super) fn mount_help(&mut self) {
        use crate::realm::components::help::Help;

        if self.modal_stack.last() == Some(&Id::Help) {
            return;
        }
        // Help reads from `ActionDef::all()` — the single source of
        // truth. Every action surfaces, grouped by section. Previously
        // each pane's `keymap()` was stitched in here with a separate
        // hand-curated GLOBAL block, which is how `g` (sidebar refresh)
        // shipped without ever appearing in the help. Now adding an
        // entry to the catalog automatically surfaces it.
        self.mount_modal(
            Id::Help,
            Help::from_catalog(&self.catalog, self.ui_defaults.terminal_escape_char),
        );
    }

    /// Build + mount the "Ask Lazybox" modal (#302): fuzzy search over
    /// a snapshot of the runtime catalog, plus the shared help
    /// conversation for agent answers. Idempotent like `mount_help`.
    pub(super) fn mount_help_ask(&mut self) {
        use crate::realm::components::help_ask::HelpAsk;

        if self.modal_stack.last() == Some(&Id::HelpAsk) {
            return;
        }
        self.mount_modal(
            Id::HelpAsk,
            HelpAsk::new(
                self.catalog.clone(),
                self.help_convo.clone(),
                self.ui_defaults.terminal_escape_char,
            ),
        );
    }

    /// Build + mount the debug / sync-status window from the current
    /// `SyncLog` snapshot. Idempotent: re-pressing the key while it's
    /// up is a no-op.
    pub(super) fn mount_sync_status(&mut self) {
        use crate::realm::components::sync_status::SyncStatus;

        if self.modal_stack.last() == Some(&Id::SyncStatus) {
            return;
        }
        let summary = self.status.sync.latest_per_source();
        let recent: Vec<_> = self.status.sync.recent().cloned().collect();
        self.mount_modal(
            Id::SyncStatus,
            SyncStatus::new(summary, recent, chrono::Utc::now())
                .with_governor(self.status.github_governor.clone()),
        );
    }

    /// Build + mount the scrollable full-description reader (#448) for a
    /// raw markdown `body` under `title`. Idempotent: re-triggering
    /// while it's up is a no-op. The modal renders proper markdown and
    /// owns no pending model state, so dismiss just pops it.
    pub(crate) fn mount_description_modal(&mut self, title: String, body: String) {
        use crate::realm::components::markdown_modal::MarkdownModal;

        if self.modal_stack.last() == Some(&Id::DescriptionModal) {
            return;
        }
        self.mount_modal(Id::DescriptionModal, MarkdownModal::new(title, body));
    }

    /// Build + mount the notices-log window from the current
    /// `MessageLog` snapshot (#309). Idempotent: re-pressing the key
    /// while it's up is a no-op. Re-mounting after a `c` clear is
    /// intentional — it rebuilds the window against the now-empty log.
    pub(super) fn mount_messages(&mut self) {
        use crate::realm::components::messages::Messages;

        if self.modal_stack.last() == Some(&Id::Messages) {
            return;
        }
        let entries: Vec<_> = self.status.messages.recent().cloned().collect();
        self.mount_modal(Id::Messages, Messages::new(entries, chrono::Utc::now()));
    }

    /// Open the durable Error Inbox (#831). Mounts immediately in a
    /// loading state and asks the daemon for the persisted snapshot; the
    /// answer (`Event::ErrorInbox`) repaints it via
    /// [`Model::update_error_inbox`].
    pub(super) fn mount_error_inbox(&mut self) {
        use crate::realm::components::error_inbox::ErrorInbox;

        if self.modal_stack.last() == Some(&Id::ErrorInbox) {
            return;
        }
        self.mount_modal(
            Id::ErrorInbox,
            ErrorInbox::new(Vec::new(), chrono::Utc::now(), true),
        );
        self.send_cmd(lazybox_ipc::Command::ListErrors);
    }

    /// Mount the confirm gate for the Error Inbox `c` (clear-all).
    /// Stacks on top of the open inbox; `Msg::Confirmed(true)` (handled
    /// in `handle_confirmed` under `Id::ErrorInboxClearConfirm`) sends
    /// `Command::ClearErrors`. Default No — an irreversible wipe should
    /// never ride a reflexive Enter.
    pub(super) fn mount_error_inbox_clear_confirm(&mut self) {
        use crate::realm::components::confirm::Confirm;

        if matches!(self.modal_stack.last(), Some(Id::ErrorInboxClearConfirm)) {
            return;
        }
        let modal = Confirm::new(
            "Clear the entire durable Error Inbox? This permanently deletes every recorded error class.",
        )
        .default_no();
        self.mount_modal(Id::ErrorInboxClearConfirm, modal);
    }

    /// Repaint a live Error Inbox with a fresh daemon snapshot. A
    /// snapshot that arrives after the window was closed is dropped.
    pub(super) fn update_error_inbox(&mut self, errors: Vec<lazybox_ipc::ErrorInboxRecord>) {
        use crate::realm::components::error_inbox::ErrorInbox;

        if self.modal_stack.last() != Some(&Id::ErrorInbox) {
            return;
        }
        self.mount_modal(
            Id::ErrorInbox,
            ErrorInbox::new(errors, chrono::Utc::now(), false),
        );
    }

    /// `i` in the Error Inbox — open a pre-filled GitHub *new issue*
    /// form in the browser, deriving the repo from the error's
    /// workspace key. Pre-filling the form (rather than creating the
    /// issue outright) keeps the user in the loop to edit before
    /// submitting — the "confirm-with-preview" spirit, with GitHub's own
    /// form as the preview.
    pub(super) fn error_inbox_file_issue(&mut self, record: lazybox_ipc::ErrorInboxRecord) {
        let Some(repo) = record
            .workspace_key
            .as_deref()
            .and_then(repo_from_workspace_key)
        else {
            self.flash_info("no repo context on this error — can't draft an issue");
            return;
        };
        let title = format!("Error: {}", truncate(&record.message, 80));
        let body = error_issue_body(&record);
        let url = format!(
            "https://github.com/{repo}/issues/new?title={}&body={}",
            percent_encode(&title),
            percent_encode(&body),
        );
        let browser = self.ui_defaults.browser.clone();
        match lazybox_tui_core::editors::open_url(&url, browser.as_deref()) {
            Ok(()) => self.flash_info(format!("drafting issue on {repo}…")),
            Err(e) => self.flash_error(format!("open failed: {e}")),
        }
    }

    /// `a` in the Error Inbox — spawn the default agent on the error's
    /// workspace with the error class as its opening brief, dogfooding
    /// lazybox on its own error stream (error → agent → PR). Closes the
    /// inbox so focus follows the new terminal.
    pub(super) fn error_inbox_route_to_agent(&mut self, record: lazybox_ipc::ErrorInboxRecord) {
        let Some(ws) = record.workspace_key.as_deref() else {
            self.flash_info("no workspace context on this error — can't route to an agent");
            return;
        };
        let session_key: lazybox_core::SessionKey = ws.into();
        let agent = self.sidebar.default_agent().to_string();
        let prompt = error_agent_prompt(&record);
        self.spawn_follow_to = Some(session_key.clone());
        self.send_cmd(lazybox_ipc::Command::Spawn {
            session_key,
            session_id: None,
            client_request_id: None,
            kind: lazybox_ipc::TerminalKind::Agent(agent.clone()),
            cwd: None,
            initial_prompt: Some(prompt),
            on_main: false,
            model_alias: None,
            access: lazybox_ipc::AgentRunAccess::Default,
        });
        if self.modal_stack.last() == Some(&Id::ErrorInbox) {
            self.pop_modal();
        }
        self.flash_info(format!("routing to {agent} on {ws}…"));
    }

    /// `x` in the Error Inbox — dump the (filtered) error set as JSONL
    /// to `~/.lazybox/v2/errors-export.jsonl` for external analysis.
    pub(super) fn error_inbox_export(&mut self, records: Vec<lazybox_ipc::ErrorInboxRecord>) {
        let Some(path) = super::home_dir().map(|h| h.join(".lazybox/v2/errors-export.jsonl"))
        else {
            self.flash_error("export failed: no home directory");
            return;
        };
        let mut out = String::new();
        for r in &records {
            match serde_json::to_string(r) {
                Ok(line) => {
                    out.push_str(&line);
                    out.push('\n');
                }
                Err(e) => {
                    self.flash_error(format!("export failed: {e}"));
                    return;
                }
            }
        }
        match std::fs::write(&path, out) {
            Ok(()) => self.flash_info(format!(
                "exported {} errors → {}",
                records.len(),
                path.display()
            )),
            Err(e) => self.flash_error(format!("export failed: {e}")),
        }
    }

    pub(super) fn queue_agent_auth_prompt(&mut self, prompt: super::AgentAuthPrompt) {
        let already_active = matches!(
            self.modal_flow,
            Some(ModalFlow::AgentAuth { terminal_id, .. }) if terminal_id == prompt.terminal_id
        );
        if already_active
            || self
                .auth_prompt_queue
                .iter()
                .any(|queued| queued.terminal_id == prompt.terminal_id)
        {
            return;
        }
        self.auth_prompt_queue.push_back(prompt);
        self.maybe_mount_next_auth_prompt();
    }

    pub(super) fn maybe_mount_next_auth_prompt(&mut self) {
        use crate::realm::components::confirm::Confirm;

        if !self.modal_stack.is_empty() {
            return;
        }
        let Some(prompt) = self.auth_prompt_queue.pop_front() else {
            return;
        };
        let copy = if prompt.retry {
            format!(
                "{} sign-in did not complete.\n\n{}\n\nThe conversation is still saved and can be resumed.\n\n[Enter] Retry    [Esc] Cancel",
                prompt.display_name,
                prompt.error.as_deref().unwrap_or("Provider login failed.")
            )
        } else {
            let affected = if prompt.credentials_isolated {
                format!(
                    "Only this agent is affected — every other {} session keeps its own login.",
                    prompt.display_name
                )
            } else if prompt.other_session_count == 0 {
                format!(
                    "This changes the machine-wide {} login.",
                    prompt.display_name
                )
            } else {
                format!(
                    "This changes the machine-wide {} login and may affect {} other running {} session{}.",
                    prompt.display_name,
                    prompt.other_session_count,
                    prompt.display_name,
                    if prompt.other_session_count == 1 {
                        ""
                    } else {
                        "s"
                    }
                )
            };
            format!(
                "{} authentication is no longer valid.\n\nSign in with another account and continue this conversation?\n\n{affected}\n\n[Enter] Sign in and continue    [Esc] Not now",
                prompt.display_name
            )
        };
        self.set_modal_flow(ModalFlow::AgentAuth {
            terminal_id: prompt.terminal_id,
            retry: prompt.retry,
        });
        self.mount_modal(Id::AgentAuth, Confirm::new(copy).default_yes());
    }

    /// If there's a queued workspace-removal prompt and no modal is
    /// currently up, mount it. Copy depends on the prompt's
    /// [`super::RemovalReason`]; the user's answer (Y → remove, N → keep +
    /// stop re-asking, Esc → defer) is handled in the
    /// `Msg::Confirmed` / `Msg::ModalDismissed` arms.
    pub(super) fn maybe_mount_next_removal_prompt(&mut self) {
        use super::RemovalReason;
        use crate::realm::components::confirm::Confirm;

        if !self.modal_stack.is_empty() {
            return;
        }
        let Some(prompt) = self.removal_prompt_queue.pop_front() else {
            return;
        };
        let copy = match prompt.reason {
            RemovalReason::OutOfScope => out_of_scope_copy(&prompt),
            RemovalReason::Merged => terminal_removal_copy(&prompt, "merged"),
            RemovalReason::Closed => terminal_removal_copy(&prompt, "closed"),
        };
        // Event path: this prompt popped unsolicited (a merged/closed
        // task, or a scope change), so a stray Enter must not delete a
        // worktree by reflex (issue #525). The
        // `ui.confirm_default.event` knob (default No) drives it.
        let modal = Confirm::new(copy);
        let modal = if self.ui_defaults.confirm_default.event.is_yes() {
            modal.default_yes()
        } else {
            modal.default_no()
        };
        self.set_modal_flow(ModalFlow::RemovalPrompt {
            workspace: prompt.workspace_key,
            reason: prompt.reason,
        });
        self.mount_modal(Id::RemoveOutOfScope, modal);
    }

    /// Drop any queued or mounted workspace-removal prompt for `key`
    /// (issue #552 reopen-cancel). Prunes the pending queue; if the
    /// active removal confirm is this workspace's, clears the binding so
    /// a later confirm can't remove the now-reopened workspace, and — if
    /// that confirm is on top of the stack — unmounts it and surfaces the
    /// next queued prompt. Clearing the flow even when the confirm is
    /// buried under another modal keeps `Msg::Confirmed` a no-op for it
    /// (it reads `ModalFlow::RemovalPrompt`), so a stale buried prompt
    /// can't silently destroy the reopened workspace.
    pub(super) fn cancel_removal_prompt(&mut self, key: &lazybox_core::WorkspaceKey) {
        self.removal_prompt_queue
            .retain(|p| &p.workspace_key != key);
        if matches!(
            &self.modal_flow,
            Some(ModalFlow::RemovalPrompt { workspace, .. }) if workspace == key
        ) {
            self.modal_flow = None;
            if self.modal_stack.last() == Some(&Id::RemoveOutOfScope) {
                self.pop_modal();
                self.maybe_mount_next_removal_prompt();
            }
        }
    }

    /// Mount the `x a` adopt-target picker. Lists every other
    /// workspace the user could move sessions into. No-op when there
    /// are no other workspaces — show a hint instead since there's
    /// nothing to pick.
    /// Unified Confirm-modal mount for any destructive catalog
    /// action. Stashes the action in `ModalFlow::ActionConfirm`;
    /// `Msg::Confirmed(true)` reads it back, dispatches via
    /// `dispatch_action_unchecked`, and drains IPC commands.
    /// `Msg::Confirmed(false)` (or Esc) drops the stash silently.
    ///
    /// The body text comes from the catalog
    /// (`ActionDef::confirm_prompt`) — keeps prompt copy in the
    /// same place as the destructive flag, so adding a new
    /// destructive action is one catalog entry, not "remember to
    /// add a prompt".
    pub(super) fn mount_action_confirm(
        &mut self,
        action: lazybox_tui_core::action::Action,
        targets: Vec<super::ActionConfirmTarget>,
        override_prompt: Option<String>,
    ) {
        use crate::realm::components::confirm::Confirm;
        use lazybox_tui_core::action::ActionDef;
        // Override wins so callers can render context-sensitive copy
        // (e.g. "Delete project X with 3 workspaces" vs. the generic
        // "Archive the focused workspace"). Catalog default is the
        // safety net when no override is available.
        let def = ActionDef::for_action(&action);
        let prompt: String = override_prompt.unwrap_or_else(|| {
            def.confirm_prompt()
                .unwrap_or("Confirm action?")
                .to_string()
        });
        // Shortcut path: the user pressed a destructive chord, so the
        // chord itself is the intent and Enter confirms (issue #525),
        // governed by `ui.confirm_default.destructive_shortcut` (default
        // Yes). A benign awareness gate (the on-main spawn) destroys
        // nothing, so it always affirms regardless of that knob.
        let default_yes = def.confirm_is_benign_gate()
            || self
                .ui_defaults
                .confirm_default
                .destructive_shortcut
                .is_yes();
        self.set_modal_flow(ModalFlow::ActionConfirm { action, targets });
        let modal = Confirm::new(&prompt);
        let modal = if default_yes {
            modal.default_yes()
        } else {
            modal.default_no()
        };
        self.mount_modal(Id::ActionConfirm, modal);
    }

    /// Offer the one-key merge-conflict resolve flow (issue #947).
    /// Mounted when `g m` is blocked (or GitHub-rejected) by conflicts:
    /// instead of a dead-end error, prompt to spawn/attach the agent
    /// with the conflict-resolution flow. The target workspace is
    /// stashed at mount time so a cursor drift under the prompt can't
    /// redirect the resolve to another PR. No-op if the workspace has
    /// been retired since the merge attempt.
    pub(super) fn mount_conflict_resolve(
        &mut self,
        workspace: &lazybox_core::WorkspaceKey,
        pr_label: &str,
    ) {
        use crate::realm::components::confirm::Confirm;
        let session_key = lazybox_core::SessionKey::from(workspace);
        if self.sidebar.workspace_by_key(&session_key).is_none() {
            return;
        }
        // Async mount — this is the `PrMergeFailed { conflict }` reply to
        // a `g m` press, which can land seconds later while the user has
        // opened something else. Mounting a `default_yes()` confirm on top
        // would steal keyboard focus and let a buffered/queued Enter spawn
        // the resolution agent unprompted, so follow the same
        // wait-for-empty-stack rule every other async daemon mount uses
        // (labels, inspect, import): any modal already up wins, and the
        // offer is dropped. The CONFLICT pill on the row stays accurate,
        // so `g m` re-triggers it — the hint says so.
        if let Some(top) = self.modal_stack.last() {
            tracing::info!(
                ?top,
                "conflict-resolve prompt skipped — another modal owns the stack"
            );
            self.flash_hint("merge conflicts — close this dialog and press g m to resolve");
            return;
        }
        let prompt = format!(
            "{pr_label} has merge conflicts — resolve them?\n\n\
             [Y] spawns an agent in the worktree to bring the branch current \
             with its base and fix the conflicts. Esc dismisses."
        );
        self.set_modal_flow(ModalFlow::ConflictResolve {
            workspace: session_key,
        });
        self.mount_modal(Id::ConflictResolve, Confirm::new(prompt).default_yes());
    }

    /// Turn an action the help agent proposed (#353) into a
    /// confirm-with-preview. Validates the payload at this boundary —
    /// a bad snippet key/body or an off-allowlist config edit is
    /// rejected with a conversation notice, never a modal — and only
    /// surfaces the confirm while the user is still on the help modal
    /// that produced it (the run outlives the modal, so a confirm
    /// popping up long after they closed it would be jarring).
    pub(super) fn propose_help_action(&mut self, intent: lazybox_tui_core::help::HelpActionIntent) {
        use crate::realm::components::confirm::Confirm;
        use lazybox_tui_core::help::HelpActionIntent;

        if self.modal_stack.last() != Some(&Id::HelpAsk) {
            return;
        }
        // `skill_root` is the resolved destination for a `scaffold_skill`
        // proposal; it rides into `ModalFlow` so apply writes exactly
        // where this preview said, regardless of later selection changes.
        let (preview, default_yes, skill_root) = match &intent {
            HelpActionIntent::AddSnippet {
                key,
                category,
                description,
                body,
            } => {
                let key = key.trim();
                if key.is_empty() || key.chars().any(char::is_whitespace) || body.trim().is_empty()
                {
                    self.reject_help_action(
                        "the assistant proposed a snippet with an invalid key or empty \
                         body — nothing was written",
                    );
                    return;
                }
                let replaces = self.snippets.get(key).is_some();
                let path = lazybox_config::Snippets::default_global_path();
                let mut preview = format!(
                    "{} snippet `{key}` in {}?\n\n",
                    if replaces { "Replace" } else { "Add" },
                    path.display(),
                );
                if !category.trim().is_empty() {
                    preview.push_str(&format!("category: {}\n", category.trim()));
                }
                if !description.trim().is_empty() {
                    preview.push_str(&format!("description: {}\n", description.trim()));
                }
                preview.push('\n');
                preview.push_str(&snippet_body_preview(body));
                preview.push_str(&format!(
                    "\n\nApplied live — send it with ]]s{key}, no restart."
                ));
                // Default Yes for a brand-new key (the user asked for it);
                // No when it would overwrite an existing snippet.
                (preview, !replaces, None)
            }
            HelpActionIntent::EditConfig { key, value } => {
                match self.validate_config_edit(key, value) {
                    Ok(edit) => {
                        let path = lazybox_config::Config::default_path();
                        let mut preview = format!(
                            "Update {} in {}?\n\n{}",
                            edit.key,
                            path.display(),
                            edit.summary
                        );
                        preview.push_str(if edit.needs_restart {
                            "\n\nTakes effect after you restart lazybox."
                        } else {
                            "\n\nApplied live — no restart."
                        });
                        (preview, true, None)
                    }
                    Err(msg) => {
                        self.reject_help_action(msg);
                        return;
                    }
                }
            }
            HelpActionIntent::ScaffoldSkill {
                name,
                description,
                body,
            } => {
                if let Err(msg) = self.validate_skill_scaffold(name, description, body) {
                    self.reject_help_action(msg);
                    return;
                }
                let Some(root) = self.skill_scaffold_root() else {
                    self.reject_help_action(
                        "a skill is scaffolded into a repo on your machine — unavailable for a \
                         remote daemon",
                    );
                    return;
                };
                let name = name.trim();
                let path = lazybox_config::skill_md_path(root.path(), name);
                if path.exists() {
                    self.reject_help_action(format!(
                        "a skill named `{name}` already exists at {} — nothing was written",
                        path.display(),
                    ));
                    return;
                }
                let preview = skill_scaffold_preview(&root, name, description.trim(), body);
                // Default Yes — the user asked for it, and the scaffold
                // refuses (never overwrites) if the skill already exists.
                (preview, true, Some(root.path().to_path_buf()))
            }
        };
        self.set_modal_flow(ModalFlow::HelpAction { intent, skill_root });
        let modal = Confirm::new(preview);
        let modal = if default_yes {
            modal.default_yes()
        } else {
            modal.default_no()
        };
        self.mount_modal(Id::HelpActionConfirm, modal);
    }

    /// Reject a proposed help action: surface `why` as the help
    /// conversation's notice (so the user sees it under the transcript)
    /// and repaint. No modal, no state change.
    fn reject_help_action(&mut self, why: impl Into<String>) {
        self.help_convo_mut().notice = Some(why.into());
        self.redraw = true;
    }

    /// Kick off the in-app worktree inspector. Dispatches the IPC
    /// `InspectWorktrees` command and flashes a hint so the user
    /// knows the click registered. `Event::WorktreesInspected`
    /// arriving later calls [`Self::mount_inspect_list`] with the
    /// payload.
    pub(super) fn start_inspect_worktrees(&mut self) {
        self.send_cmd(lazybox_ipc::Command::InspectWorktrees);
        self.flash_info("inspecting worktrees…");
    }

    /// Mount the inspector list. Called from the
    /// `Event::WorktreesInspected` handler. Stashes the report in
    /// `ModalFlow::InspectList` so the choice handler can index back.
    ///
    /// Row layout:
    /// - sentinel "delete all N safe" row (only when N > 0)
    /// - one row per flagged orphan, with bracketed reason tags
    ///   + DIRTY/UNPUSHED markers
    /// - one row per healthy worktree, rendered non-selectable so
    ///   the user has full visibility but can't accidentally delete
    pub(super) fn mount_inspect_list(
        &mut self,
        inspections: Vec<lazybox_ipc::WorktreeInspectionDto>,
    ) {
        use crate::realm::components::choice::Choice;

        // Async mount (the `WorktreesInspected` reply) — same
        // don't-preempt rule as the label picker: only take the stack
        // when it's empty or already owned by the inspector flow
        // (loading placeholder, the list itself after a delete
        // re-inspect, or its per-row confirm). In multi-client mode
        // another client's `InspectWorktrees` broadcasts here too, and
        // mounting on top of an unrelated modal would steal focus.
        let inspector_owned = matches!(
            self.modal_stack.last(),
            None | Some(Id::InspectLoading | Id::InspectList | Id::InspectConfirm)
        );
        if !inspector_owned {
            tracing::info!("worktree inspector list skipped — another modal owns the stack");
            return;
        }

        // Sort: orphans first (sorted by path), then healthy.
        let mut rows = inspections;
        rows.sort_by(|a, b| {
            let a_orphaned = !a.reasons.is_empty();
            let b_orphaned = !b.reasons.is_empty();
            b_orphaned
                .cmp(&a_orphaned)
                .then_with(|| a.path.cmp(&b.path))
        });
        let safe_count = rows
            .iter()
            .filter(|r| !r.reasons.is_empty() && r.is_safe_to_delete)
            .count();

        // Wrap each entry in `InspectRow` so the Choice picker can
        // distinguish the bulk shortcut from a real worktree row.
        // The bulk row is only inserted when there's something to
        // bulk-delete — otherwise it would be a misleading no-op.
        let mut items: Vec<InspectRow> = Vec::with_capacity(rows.len() + 1);
        if safe_count > 0 {
            items.push(InspectRow::BulkSafe { count: safe_count });
        }
        for row in &rows {
            items.push(InspectRow::Inspection(row.clone()));
        }

        // Stash the full list so the pick handler can resolve indices
        // back to inspections (skipping the sentinel as needed). Assign
        // directly, not via `set_modal_flow`: a delete-and-re-inspect
        // legitimately replaces a still-live `InspectList` /
        // `InspectConfirm` flow (the `inspector_owned` guard above
        // permits exactly that), so the "no flow armed" assertion does
        // not apply here.
        self.modal_flow = Some(ModalFlow::InspectList { rows });

        if items.is_empty() {
            self.flash_info("no worktrees found under <state_root>/worktrees/");
            return;
        }

        let modal = Choice::single("Worktree inspector", items)
            .title("Worktree inspector")
            .label(format_inspect_row)
            .selectable(|row: &InspectRow| match row {
                InspectRow::BulkSafe { .. } => true,
                InspectRow::Inspection(dto) => !dto.reasons.is_empty(),
            });
        // Replace the previous instance (if any) so the modal stack
        // doesn't pile up after a delete + re-inspect.
        self.modal_stack.retain(|id| id != &Id::InspectList);
        self.mount_modal(Id::InspectList, modal);
    }

    /// Confirm-modal step in front of an actual delete. Stashes the
    /// target so `Msg::Confirmed(true)` knows what to dispatch. Copy
    /// changes when the row has local work so the user sees a clear
    /// "FORCE" warning before they say yes.
    pub(super) fn mount_inspect_confirm(&mut self, target: lazybox_ipc::WorktreeInspectionDto) {
        use crate::realm::components::confirm::Confirm;
        let dirty = target.has_uncommitted_changes || target.has_unpushed_commits;
        let prompt = if dirty {
            format!(
                "Delete worktree {} ? It has {}{}{} — this overrides safety.",
                target.path.display(),
                if target.has_uncommitted_changes {
                    "uncommitted changes"
                } else {
                    ""
                },
                if target.has_uncommitted_changes && target.has_unpushed_commits {
                    " AND "
                } else {
                    ""
                },
                if target.has_unpushed_commits {
                    "unpushed commits"
                } else {
                    ""
                },
            )
        } else {
            format!("Delete worktree {} ?", target.path.display())
        };
        // Deletes a worktree off disk (overriding safety when dirty) —
        // irreversible, uncommitted/unpushed work is lost. This keeps a
        // hard No floor rather than following
        // `confirm_default.destructive_shortcut` (#525): the disk-loss
        // risk warrants caution beyond the general shortcut policy, and
        // No is never less safe than that knob would ask for.
        let modal = Confirm::new(&prompt).default_no();
        self.set_modal_flow(ModalFlow::InspectConfirm { target });
        self.mount_modal(Id::InspectConfirm, modal);
    }

    /// Kick off the dev-folder scan (`x i`). Dispatches
    /// `Command::ScanCheckouts` (roots come from `scan.roots`) and
    /// flashes a hint. `Event::CheckoutsDiscovered` arriving later
    /// calls [`Self::mount_import_checkout_picker`] with the payload.
    pub(super) fn start_scan_checkouts(&mut self) {
        self.send_cmd(lazybox_ipc::Command::ScanCheckouts { roots: Vec::new() });
        self.flash_info("scanning dev folders…");
    }

    /// Mount the "add scan root" directory-path prompt (`x r`). Submit →
    /// [`Self::handle_input_submitted`] under `Id::AddScanRoot` appends
    /// the path to `scan.roots` in config and scans just that root.
    pub(super) fn mount_add_scan_root_input(&mut self) {
        use crate::realm::components::input::Input;

        if matches!(self.modal_stack.last(), Some(Id::AddScanRoot)) {
            return;
        }

        let modal = Input::new("Directory to scan for git checkouts")
            .title("Add scan root")
            .placeholder("e.g. ~/development, ~/code, /Users/you/src, …")
            .with_validator(|s: &str| !s.trim().is_empty());
        self.mount_modal(Id::AddScanRoot, modal);
    }

    /// Mount the import picker. Called from the
    /// `Event::CheckoutsDiscovered` handler. Stashes the discovered
    /// rows in `ModalFlow::ImportList` so the choice handler can index
    /// back to the picked checkout.
    pub(super) fn mount_import_checkout_picker(
        &mut self,
        checkouts: Vec<lazybox_ipc::DiscoveredCheckoutDto>,
    ) {
        use crate::realm::components::choice::Choice;

        // Async mount (the scan reply): only take the stack when it's
        // empty or already owned by the import flow, so another client's
        // scan broadcast (multi-client mode) can't steal focus from an
        // unrelated modal.
        let import_owned = matches!(
            self.modal_stack.last(),
            None | Some(Id::ImportCheckoutList | Id::ImportCheckoutConfirm)
        );
        if !import_owned {
            return;
        }
        if checkouts.is_empty() {
            self.flash_info("no importable checkouts found under scan.roots");
            return;
        }

        let labels: Vec<String> = checkouts.iter().map(format_discovered_checkout).collect();
        // Direct assign, not `set_modal_flow`: a rescan legitimately
        // replaces a still-live import flow (the `import_owned` guard
        // permits `ImportCheckoutList` / `ImportCheckoutConfirm`), so the
        // "no flow armed" assertion does not apply.
        self.modal_flow = Some(ModalFlow::ImportList { rows: checkouts });

        let modal = Choice::single("Import which checkout?", labels)
            .title("Import local checkout")
            .label(|s: &String| s.clone());
        self.modal_stack.retain(|id| id != &Id::ImportCheckoutList);
        self.mount_modal(Id::ImportCheckoutList, modal);
    }

    /// Confirm step in front of an actual import. Warns that sessions
    /// run in the user's REAL checkout — not an isolated worktree — so
    /// an agent started here edits the real tree, mirroring the `b`
    /// on-main warning. Surfaces uncommitted state when the checkout is
    /// dirty. Stashes the target so `Msg::Confirmed(true)` dispatches
    /// `ImportLocalCheckout`.
    pub(super) fn mount_import_checkout_confirm(
        &mut self,
        target: lazybox_ipc::DiscoveredCheckoutDto,
    ) {
        use crate::realm::components::confirm::Confirm;
        let repo = target.repo.as_deref().unwrap_or("(no GitHub origin)");
        let dirty = if target.has_uncommitted_changes {
            " It has uncommitted changes — lazybox won't touch them, but an agent might."
        } else {
            ""
        };
        let prompt = format!(
            "Import {repo} at {} as a linked workspace? Sessions run in this \
             REAL checkout on its current branch — no isolated worktree — so \
             agents/shells edit it directly.{dirty}",
            target.path.display(),
        );
        let modal = Confirm::new(&prompt).default_yes();
        self.set_modal_flow(ModalFlow::ImportConfirm { target });
        self.mount_modal(Id::ImportCheckoutConfirm, modal);
    }

    /// Confirm prompt before dispatching `Command::CleanWorktrees`.
    /// The destructive bit is on disk — sessions + their worktrees
    /// are gone after this. PR/issue rows stay because we only
    /// touch session records. `Msg::Confirmed(true)` fires the IPC;
    /// `(false)` / dismiss drops the prompt silently.
    pub(super) fn mount_clean_worktrees_confirm(&mut self) {
        use crate::realm::components::confirm::Confirm;
        // Bulk-wipes worktrees off disk — irreversible. Like the
        // per-worktree inspector delete, this keeps a hard No floor
        // instead of following `confirm_default.destructive_shortcut`
        // (#525): a mis-hit here could wipe many trees at once.
        let modal = Confirm::new(
            "Wipe every worktree whose session has no live terminal? \
             PR / issue rows stay; active sessions are skipped.",
        )
        .default_no();
        self.mount_modal(Id::CleanWorktreesConfirm, modal);
    }

    /// Build the action list for a right-click on a sidebar
    /// workspace row, then mount a Choice modal to pick one. The
    /// menu only offers actions that *make sense* for this row —
    /// e.g. `MergePr` only when the PR is in a merge-ready state —
    /// so the user never sees a no-op entry.
    pub(super) fn mount_sidebar_context_menu(&mut self, session_key: lazybox_core::SessionKey) {
        use crate::realm::components::choice::Choice;
        use lazybox_tui_core::action::{Action, ActionDef, ActionKind, availability};

        // Snapshot the workspace state — the catalog's `availability`
        // resolver takes `Option<&Workspace>` and decides whether each
        // action makes sense for this row. No bespoke gating logic
        // duplicated here — `availability` defers to the same
        // `intent::*` resolvers the keyboard path uses.
        let workspace = self.sidebar.workspace_by_key(&session_key).cloned();

        // Catalog-driven menu: list every workspace-scoped action,
        // then filter by `availability`. Adding a new action to
        // the catalog automatically surfaces it here (gated by its
        // resolver) — no more "remembered to wire menu but forgot
        // help."
        let candidates: Vec<Action> = vec![
            Action::SpawnAgent("claude".into()),
            Action::SpawnShell,
            Action::OpenEditor,
            Action::MarkAllRead,
            Action::MergePr,
            Action::Archive,
        ];
        let actions: Vec<Action> = candidates
            .into_iter()
            .filter(|a| {
                if !availability(a.kind(), workspace.as_ref()) {
                    return false;
                }
                // Surface-specific extra gate: don't offer "open
                // editor" when no editor was detected at startup.
                // The catalog can't know about lazybox's setup state,
                // so the menu enforces this one.
                if a.kind() == ActionKind::OpenEditor && self.setup.editors.is_empty() {
                    return false;
                }
                true
            })
            .collect();

        // Labels format: `<verb>  (<key>)` — same as before, just
        // sourced from the catalog so a default_keys remap flows
        // through automatically. `SpawnAgent("claude")` overrides
        // the static "spawn agent" label with the actual agent id.
        let labels: Vec<String> = actions
            .iter()
            .map(|a| {
                let def = ActionDef::for_action(a);
                let verb = match a {
                    Action::SpawnAgent(id) => format!("spawn {id}"),
                    _ => def.label.to_string(),
                };
                format!("{verb}  ({})", def.default_keys)
            })
            .collect();

        self.set_modal_flow(ModalFlow::SidebarContext {
            session_key,
            actions,
        });
        let modal = Choice::single("Actions", labels)
            .title("Workspace actions")
            .label(|s: &String| s.clone());
        self.mount_modal(Id::SidebarContext, modal);
    }

    pub(super) fn mount_adopt_picker(&mut self, source_key: lazybox_core::WorkspaceKey) {
        use crate::realm::components::choice::Choice;

        // Build (target_key, label) pairs from every workspace EXCEPT
        // the source. Labels prefer the primary task's `owner/repo#N`
        // form so the picker reads like the inbox rows.
        let mut items: Vec<(lazybox_core::WorkspaceKey, String)> = Vec::new();
        for (key, ws) in self.sidebar.workspace_iter() {
            if key.as_str() == source_key.as_str() {
                continue;
            }
            let label = ws
                .primary_task()
                .map(|t| t.id.key.clone())
                .unwrap_or_else(|| ws.name.clone());
            items.push((lazybox_core::WorkspaceKey::new(key.as_str()), label));
        }
        if items.is_empty() {
            self.flash_info("no other workspace to adopt sessions into");
            return;
        }
        self.set_modal_flow(ModalFlow::AdoptSource { source: source_key });

        // Each row carries its workspace key (#512).
        type AdoptRow = (lazybox_core::WorkspaceKey, String);
        let modal = Choice::single("Move sessions to which workspace?", items)
            .title("Adopt sessions")
            .label(|(_, l): &AdoptRow| l.clone())
            .payload_for(|(k, _): &AdoptRow| ChoicePayload::Workspace(k.clone()));
        self.mount_modal(Id::AdoptTarget, modal);
    }

    /// Mount the agent-to-agent handoff target picker (`x s`, issue
    /// #431). Candidates are every OTHER workspace running an agent —
    /// excluding the source, so a handoff can't loop straight back to
    /// itself, and excluding shell-only workspaces since the brief is
    /// meant for another agent, not a shell prompt. The source name +
    /// the seed captured from its agent screen are stashed in
    /// `ModalFlow::Handoff`; the pick funnels into the compose textarea
    /// (`mount_handoff_textarea`). No eligible target → a footer nudge
    /// and nothing stashed.
    pub(super) fn mount_handoff_picker(
        &mut self,
        source_key: &lazybox_core::SessionKey,
        source_name: String,
        seed: String,
    ) {
        use crate::realm::components::choice::Choice;

        let mut items: Vec<(lazybox_core::SessionKey, String)> = Vec::new();
        for (key, ws) in self.sidebar.workspace_iter() {
            if key.as_str() == source_key.as_str() {
                continue;
            }
            let label = ws
                .primary_task()
                .map(|t| t.id.key.clone())
                .unwrap_or_else(|| ws.name.clone());
            items.push((key.clone(), label));
        }
        // Only workspaces whose deliverable terminal is an agent —
        // `broadcast_terminal` returns `is_agent`, and the handoff brief
        // is meant for another agent, not a shell.
        items.retain(|(key, _)| matches!(self.sidebar.broadcast_terminal(key), Some((_, true))));
        if items.is_empty() {
            self.flash_info("no other running agent to hand off to");
            return;
        }
        self.set_modal_flow(ModalFlow::Handoff {
            draft: HandoffDraft {
                source: source_key.clone(),
                source_name,
                seed,
                target: None,
            },
        });

        // Each row carries its session key (#512).
        type HandoffRow = (lazybox_core::SessionKey, String);
        let modal = Choice::single("Send this agent's output to which session?", items)
            .title("Send to session")
            .label(|(_, l): &HandoffRow| l.clone())
            .payload_for(|(k, _): &HandoffRow| ChoicePayload::Session(k.clone()));
        self.mount_modal(Id::HandoffTarget, modal);
    }

    /// Mount the handoff compose step: a Textarea pre-filled with the
    /// captured source-agent output, headed with the "source → target"
    /// trail. Submit injects the (edited) body into the target session
    /// (`dispatch_handoff`).
    pub(super) fn mount_handoff_textarea(&mut self) {
        use crate::realm::components::textarea::Textarea;
        let Some(ModalFlow::Handoff { draft }) = &self.modal_flow else {
            return;
        };
        let Some(target) = &draft.target else {
            return;
        };
        let target_name = self
            .sidebar
            .workspace_by_key(target)
            .map(|w| w.name.clone())
            .unwrap_or_else(|| target.to_string());
        let header = format!("Handoff {} → {}", draft.source_name, target_name);
        let mut modal = Textarea::new("Send to session").with_header(header);
        if !draft.seed.is_empty() {
            // Trailing blank line so any note the user appends starts on
            // its own line; trimmed back off at send time if unused.
            modal = modal.with_body(format!("{}\n\n", draft.seed.trim_end()));
        }
        self.mount_modal(Id::HandoffText, modal);
    }

    pub(super) fn mount_conversion_role_picker(&mut self, draft: ConversionDraft) {
        use crate::realm::components::choice::Choice;
        use lazybox_core::prompts::AgentHandoffRole;

        self.set_modal_flow(ModalFlow::ConvertSession { draft });
        let modal = Choice::single(
            "Continue resets context · Critic reviews without editing",
            vec![AgentHandoffRole::Continue, AgentHandoffRole::Critic],
        )
        .title("Convert session")
        .label(|role: &AgentHandoffRole| role.label().to_string())
        .payload_for(|role: &AgentHandoffRole| ChoicePayload::HandoffRole(*role));
        self.mount_modal(Id::ConvertSessionRole, modal);
    }

    /// Drive the global "start agent" (`Shift-W`) flow. Resolve the
    /// project set up front:
    ///
    /// - **No projects** → footer nudge pointing at `x p`; there's
    ///   nothing to create a workspace under yet.
    /// - **One project** → skip the picker and go straight to the
    ///   name input (the project is unambiguous).
    /// - **Several** → mount the project picker; the pick funnels into
    ///   the same name input.
    ///
    /// The name input's submit auto-spawns the configured default
    /// agent (see `handle_input_submitted`), so this whole flow is
    /// "create workspace + start agent" in one keystroke chain.
    pub(crate) fn start_agent_flow(&mut self) {
        let projects = self.sidebar.projects_for_picker();
        match projects.len() {
            0 => {
                self.flash_info("no projects yet — create one with x p");
            }
            1 => {
                let (key, _) = projects.into_iter().next().expect("len checked == 1");
                self.mount_new_workspace_input(key);
            }
            _ => self.mount_start_agent_picker(projects),
        }
    }

    /// Mount the project picker for the `Shift-W` start-agent flow.
    /// Mirrors `mount_adopt_picker`: stash the keys in row order so
    /// `Msg::ChoicePicked` can recover the chosen `ProjectKey`.
    fn mount_start_agent_picker(&mut self, projects: Vec<(lazybox_core::ProjectKey, String)>) {
        use crate::realm::components::choice::Choice;

        // Each row carries its project key (#512).
        type ProjectRow = (lazybox_core::ProjectKey, String);
        let modal = Choice::single("Start agent in which project?", projects)
            .title("Start agent")
            .label(|(_, name): &ProjectRow| name.clone())
            .payload_for(|(k, _): &ProjectRow| ChoicePayload::Project(k.clone()));
        self.mount_modal(Id::StartAgentProject, modal);
    }

    /// Surface the next queued issue→PR merge prompt when no modal
    /// is currently up. The user's answer drives `Msg::Confirmed` /
    /// `Msg::ModalDismissed`, which dispatch a `Command::ConfirmMerge`
    /// back to the daemon. Default-yes: the prompt only appears because
    /// a PR was detected closing this issue, so joining its sessions in
    /// is the expected path — and it's non-destructive (the sessions
    /// move, nothing is lost). Declining is the surprising outcome, so
    /// Enter affirms the join.
    pub(super) fn maybe_mount_next_merge_prompt(&mut self) {
        use crate::realm::components::confirm::Confirm;

        if !self.modal_stack.is_empty() {
            return;
        }
        let Some((issue_key, pr_key, issue_label, pr_label, count)) =
            self.merge_prompt_queue.pop_front()
        else {
            return;
        };
        // Event-driven (a closing PR was detected), but benign: joining
        // the issue's sessions into the PR workspace destroys nothing,
        // and accepting is the expected path. So it defaults Yes and is
        // exempt from `confirm_default.event` — that knob guards the
        // *destructive* unsolicited prompts (worktree removal), not this
        // one (#525).
        let modal =
            Confirm::new(merge_prompt_question(&pr_label, &issue_label, count)).default_yes();
        self.set_modal_flow(ModalFlow::MergePrompt {
            issue: issue_key,
            pr: pr_key,
        });
        self.mount_modal(Id::MergeConfirm, modal);
    }

    /// Route a `WorktreeProgress` event by spawn origin (issue #645).
    /// A user-initiated spawn mounts the progress checklist modal; an
    /// autonomous (GitHub label / `@lazybox` mention) spawn is
    /// background work the user didn't ask for, so it reports a one-line
    /// footer notice and provisions quietly — never grabbing focus with
    /// a modal. A genuine *failure* still routes to the checklist modal
    /// regardless of origin, since it carries the recovery affordance
    /// (#594) and needs a decision.
    pub(super) fn route_worktree_progress(
        &mut self,
        session_key: lazybox_core::SessionKey,
        step: lazybox_ipc::WorktreeStep,
        status: lazybox_ipc::WorktreeStepStatus,
        origin: lazybox_ipc::SpawnOrigin,
    ) {
        let trigger = match origin {
            lazybox_ipc::SpawnOrigin::Interactive => {
                self.apply_worktree_progress(session_key, step, status);
                return;
            }
            lazybox_ipc::SpawnOrigin::Autonomous(trigger) => trigger,
        };
        if matches!(status, lazybox_ipc::WorktreeStepStatus::Failed(_)) {
            // A failed background provision needs the recovery modal.
            self.autonomous_spawn_notified.remove(&session_key);
            self.apply_worktree_progress(session_key, step, status);
            return;
        }
        // Provisioning finished (the terminal `Setup` step reached
        // `Done`) — drop the marker so a later re-spawn on this
        // workspace announces again.
        let finished = matches!(step, lazybox_ipc::WorktreeStep::Setup)
            && matches!(status, lazybox_ipc::WorktreeStepStatus::Done);
        // `insert` is true only on the first event for this spawn, so
        // the notice fires once across the several steps a provision
        // emits.
        if self.autonomous_spawn_notified.insert(session_key.clone()) {
            self.flash_info(format!(
                "starting agent on {} ({})",
                worktree_notice_label(&session_key),
                trigger.notice_tag()
            ));
        }
        if finished {
            self.autonomous_spawn_notified.remove(&session_key);
        }
    }

    /// Fold one `WorktreeProgress` daemon event into the checklist and
    /// (re)mount the modal. Mounted lazily on the first event so an
    /// instant resume — which provisions nothing and emits no events —
    /// never flashes the modal. Re-mounting on each step keeps the
    /// checklist advancing in place; the `retain` guards against piling
    /// duplicate ids onto the stack.
    pub(super) fn apply_worktree_progress(
        &mut self,
        session_key: lazybox_core::SessionKey,
        step: lazybox_ipc::WorktreeStep,
        status: lazybox_ipc::WorktreeStepStatus,
    ) {
        use crate::realm::components::worktree_progress::{
            WorktreeProgress, WorktreeProgressState,
        };
        // Esc'd checklist: the user already dismissed this operation's
        // modal, so its later progress events must NOT resurrect it on
        // top of whatever they're typing (pre-fix every step re-mounted
        // with `app.active`, yanking keyboard focus back). Absorb the
        // update silently — except a *failed* step, which still
        // surfaces as a footer error so a dismissed checklist can't
        // hide a broken provision. A DIFFERENT session starting to
        // provision is a new operation: release the marker and show
        // its checklist normally.
        if self.worktree_progress_dismissed.as_ref() == Some(&session_key) {
            if let lazybox_ipc::WorktreeStepStatus::Failed(err) = &status {
                self.worktree_progress_dismissed = None;
                // The daemon confirming this client's own Esc-cancel
                // arrives as a `Failed` (so every client's checklist
                // stops), but it isn't an error to the user who asked
                // for it — frame it as a plain confirmation.
                if err == lazybox_ipc::SPAWN_CANCELLED_NOTE {
                    self.flash_info(lazybox_ipc::SPAWN_CANCELLED_NOTE);
                } else {
                    self.flash_error(format!("✗ worktree setup failed — {err}"));
                }
            }
            return;
        }
        self.worktree_progress_dismissed = None;
        // #1041: an unmapped Linear team surfaces as a Failed provision
        // step, but it's a missing *choice*, not a breakage. Open the repo
        // picker directly — the primary path — instead of the "× spawn
        // aborted / retry once fixed" checklist. `open_…` tears down the
        // in-flight spinner itself; it returns false (falling through to the
        // normal failed modal) only when there's genuinely no repo to
        // propose — the true last resort.
        if let lazybox_ipc::WorktreeStepStatus::Failed(message) = &status
            && lazybox_ipc::WorktreeRecovery::classify(message)
                == lazybox_ipc::WorktreeRecovery::LinearUnmapped
        {
            let message = message.clone();
            if self.open_linear_team_repo_picker(&message) {
                return;
            }
        }
        // A new spawn supersedes any stale checklist (e.g. the previous
        // one errored and the user re-pressed `w`).
        let state = match self.worktree_progress.as_mut() {
            Some(s) if s.session_key == session_key => s,
            _ => {
                self.worktree_progress = Some(WorktreeProgressState::new(session_key));
                self.worktree_progress.as_mut().expect("just assigned Some")
            }
        };
        state.apply(step, status);
        let modal = WorktreeProgress::from_state(state);
        self.modal_stack.retain(|id| id != &Id::WorktreeProgress);
        self.mount_modal(Id::WorktreeProgress, modal);
    }

    /// Queue dismissal of the worktree-progress checklist for
    /// `session_key`. The session is live (`TerminalSpawned`), but the
    /// modal is NOT torn down here — it stays up until the display has
    /// walked through every step for its minimum dwell, so a fast
    /// provision shows the full checklist instead of flashing the first
    /// step. [`Self::advance_worktree_progress`] performs the actual
    /// teardown once the display drains. A checklist sitting on a failed
    /// step is left untouched (it stays up until the user presses Esc).
    pub(super) fn queue_worktree_progress_dismiss(
        &mut self,
        session_key: &lazybox_core::SessionKey,
    ) {
        // The operation completed — release any Esc-dismissal marker
        // for it so the NEXT provision on this workspace gets its
        // checklist again.
        if self.worktree_progress_dismissed.as_ref() == Some(session_key) {
            self.worktree_progress_dismissed = None;
        }
        // Same release for the autonomous footer-notice marker. It
        // normally clears on the terminal `Setup`/`Done` step, but a
        // lagged broadcast can drop that event; a live terminal for the
        // session proves provisioning finished, so drop the marker here
        // too — otherwise a later re-spawn on this workspace would stay
        // silent (issue #645).
        self.autonomous_spawn_notified.remove(session_key);
        if let Some(state) = self.worktree_progress.as_mut()
            && &state.session_key == session_key
            && !state.failed()
        {
            state.queue_dismiss();
            // Nudge the display in case the dwell has already elapsed (a
            // slow provision), so we don't wait a whole extra tick.
            self.advance_worktree_progress();
        }
    }

    /// Backstop for #219: `TerminalSpawned` is the normal completion
    /// signal, but snapshots/reconnects or lagged event streams can
    /// prove the same fact by showing a live terminal for the
    /// checklist's session. Queue the same graceful dismissal from that
    /// projected state so the modal cannot remain stuck on clone after
    /// the work actually completed.
    pub(super) fn reconcile_worktree_progress_with_terminals(&mut self) {
        let Some(session_key) = self.worktree_progress.as_ref().and_then(|state| {
            if !state.failed() && self.terminals.terminal_count_for(&state.session_key) > 0 {
                Some(state.session_key.clone())
            } else {
                None
            }
        }) else {
            return;
        };
        self.queue_worktree_progress_dismiss(&session_key);
    }

    /// Walk the displayed checklist one step toward the daemon's truth
    /// (gated by the min-dwell) and re-mount the modal if it changed.
    /// Tears the modal down once a queued dismiss has been fully shown.
    /// Driven by the per-tick `Msg::WorktreeProgressTick`.
    pub(super) fn advance_worktree_progress(&mut self) {
        self.advance_worktree_progress_at(std::time::Instant::now());
    }

    /// [`Self::advance_worktree_progress`] with an injectable clock so
    /// tests can drive the min-dwell walk deterministically instead of
    /// sleeping. Production always passes `Instant::now()`.
    pub(super) fn advance_worktree_progress_at(&mut self, now: std::time::Instant) {
        use crate::realm::components::worktree_progress::WorktreeProgress;
        let (changed, dismiss) = match self.worktree_progress.as_mut() {
            Some(state) => {
                let changed = state.tick(now);
                (changed, state.ready_to_dismiss())
            }
            None => return,
        };
        if dismiss {
            self.force_dismiss_worktree_progress();
        } else if changed {
            let state = self
                .worktree_progress
                .as_ref()
                .expect("present: tick borrowed it");
            let modal = WorktreeProgress::from_state(state);
            self.modal_stack.retain(|id| id != &Id::WorktreeProgress);
            self.mount_modal(Id::WorktreeProgress, modal);
            self.redraw = true;
        }
    }

    /// Unconditionally tear down the worktree-progress checklist
    /// (regardless of session or failed state). Used when a spawn fails
    /// outright — the error goes to the footer and there's nothing left
    /// for the checklist to advance toward.
    pub(super) fn force_dismiss_worktree_progress(&mut self) {
        if self.worktree_progress.take().is_some() {
            self.modal_stack.retain(|id| id != &Id::WorktreeProgress);
            let _ = self.app.umount(&Id::WorktreeProgress);
            if let Some(top) = self.modal_stack.last() {
                let _ = self.app.active(top);
            }
            self.redraw = true;
        }
    }

    /// The session the last remembered spawn targets, if any — the spawn
    /// a `spawn:worktree` failure is attributed to (the provider error
    /// carries no session key of its own).
    pub(super) fn last_spawn_session_key(&self) -> Option<&lazybox_core::SessionKey> {
        match &self.last_spawn {
            Some(lazybox_ipc::Command::Spawn { session_key, .. }) => Some(session_key),
            _ => None,
        }
    }

    /// Whether a worktree-provisioning checklist is live for a *different*
    /// spawn than the one a current failure is attributed to. Such a
    /// checklist must never be torn down by an unrelated spawn's failure
    /// (finding 3 / concurrent spawns); an absent or unattributable
    /// checklist is also treated as not-ours so it is left intact.
    pub(super) fn worktree_checklist_is_foreign_and_live(&self) -> bool {
        self.worktree_progress
            .as_ref()
            .is_some_and(|s| !s.failed() && self.last_spawn_session_key() != Some(&s.session_key))
    }

    /// Route a worktree-provisioning failure that reached the client only
    /// as a `spawn:worktree` `ProviderError` — with no live progress
    /// checklist to absorb its `Failed` step — onto the recovery modal
    /// (#594). Without this a fully-classified failure (e.g.
    /// `BranchHeldLive`) fell through to a single middle-truncated footer
    /// line that elided the actionable recovery text — the exact #557/#562
    /// regression. Classifies the message once to place the ✗ on the phase
    /// that aborted and render the per-class hint + `r` retry. Returns
    /// whether the modal was mounted; `false` — no remembered spawn to
    /// attach the retry to, or a *different* spawn's checklist is still
    /// live and must not be clobbered — leaves the caller its footer
    /// fallback.
    pub(super) fn route_spawn_failure_to_recovery(&mut self, message: &str) -> bool {
        use crate::realm::components::worktree_progress::{
            WorktreeProgress, WorktreeProgressState,
        };
        let Some(lazybox_ipc::Command::Spawn { session_key, .. }) = self.last_spawn.clone() else {
            return false;
        };
        // This failure belongs to `session_key`; a live checklist for
        // another session must keep advancing rather than be replaced.
        if self
            .worktree_progress
            .as_ref()
            .is_some_and(|s| !s.failed() && s.session_key != session_key)
        {
            return false;
        }
        // #1041: an unmapped Linear team is not a failure to *show* — it's a
        // missing choice to *make*. Open the repo picker directly as the
        // primary path (persist + re-provision on pick), never the "× spawn
        // aborted / retry once fixed" dead-end. Only when there's genuinely
        // no repo to propose does it fall through to the failure modal below.
        if lazybox_ipc::WorktreeRecovery::classify(message)
            == lazybox_ipc::WorktreeRecovery::LinearUnmapped
            && self.open_linear_team_repo_picker(message)
        {
            return true;
        }
        let step = lazybox_ipc::WorktreeRecovery::classify(message).failed_step();
        let mut state = WorktreeProgressState::new(session_key);
        state.apply(
            step,
            lazybox_ipc::WorktreeStepStatus::Failed(message.to_string()),
        );
        self.worktree_progress_dismissed = None;
        self.worktree_progress = Some(state);
        let modal = WorktreeProgress::from_state(
            self.worktree_progress.as_ref().expect("just assigned Some"),
        );
        self.modal_stack.retain(|id| id != &Id::WorktreeProgress);
        self.mount_modal(Id::WorktreeProgress, modal);
        self.redraw = true;
        true
    }

    /// `r` on a failed `WorktreeProgress` modal: re-issue the spawn that
    /// failed (issue #557). A failed provision persists no session, so a
    /// clean re-send retries the whole worktree setup — after the user
    /// has (say) closed the holding worktree or reconnected. Resets the
    /// frozen checklist so the fresh spawn's progress events mount a new
    /// modal instead of reusing the stuck failed state.
    pub(super) fn retry_worktree_provision(&mut self) {
        // Only meaningful on a failed checklist with a remembered spawn.
        let is_failed = self.worktree_progress.as_ref().is_some_and(|s| s.failed());
        let Some(spawn) = self.last_spawn.clone() else {
            if is_failed {
                self.flash_hint("nothing to retry");
            }
            return;
        };
        if !is_failed {
            return;
        }
        self.force_dismiss_worktree_progress();
        // A superseded checklist would otherwise be treated as
        // Esc-dismissed; clear the marker so the retry's events mount.
        self.worktree_progress_dismissed = None;
        self.flush_dispatched_cmds(vec![spawn]);
    }

    /// Re-issue the remembered spawn after mapping an unmapped Linear team
    /// (#1041). Unlike `retry_worktree_provision`, this is not gated on a
    /// failed checklist: the picker now opens *before* any failure modal, so
    /// there is usually no failed state to observe — the pick alone must
    /// carry the spawn into the freshly-mapped repo. Any stale progress
    /// modal is torn down first so the retry's own events mount cleanly.
    pub(super) fn reprovision_after_linear_map(&mut self) {
        let Some(spawn) = self.last_spawn.clone() else {
            self.flash_hint("nothing to retry");
            return;
        };
        self.force_dismiss_worktree_progress();
        self.worktree_progress_dismissed = None;
        self.flush_dispatched_cmds(vec![spawn]);
    }

    /// `r` on a non-retryable but recoverable `WorktreeProgress` modal
    /// (issue #787): preserve the conflicting checkout aside and
    /// re-provision, then re-run the original spawn. Reuses the remembered
    /// spawn so the recreate lands the exact agent/shell the user asked
    /// for. `BranchHeldManaged` names a different-path holder to move; the
    /// other recoverable classes move the workspace's own target worktree.
    pub(super) fn recreate_worktree_provision(&mut self) {
        let is_failed = self.worktree_progress.as_ref().is_some_and(|s| s.failed());
        if !is_failed {
            return;
        }
        let Some(lazybox_ipc::Command::Spawn {
            session_key,
            session_id,
            client_request_id,
            kind,
            cwd,
            initial_prompt,
            on_main,
            model_alias,
            access,
        }) = self.last_spawn.clone()
        else {
            self.flash_hint("nothing to recreate");
            return;
        };
        let preserve_holder = self.worktree_progress.as_ref().and_then(|state| {
            matches!(
                state.recovery(),
                Some(lazybox_ipc::WorktreeRecovery::BranchHeldManaged)
            )
            .then(|| {
                state
                    .error()
                    .and_then(lazybox_ipc::WorktreeRecovery::holder_path)
            })
            .flatten()
        });
        self.force_dismiss_worktree_progress();
        self.worktree_progress_dismissed = None;
        let cmd = lazybox_ipc::Command::RecreateWorktree {
            spawn: Box::new(lazybox_ipc::SpawnFallback {
                session_key,
                session_id,
                client_request_id,
                kind,
                cwd,
                model_alias,
                access,
            }),
            initial_prompt,
            on_main,
            preserve_holder,
        };
        self.flush_dispatched_cmds(vec![cmd]);
    }

    /// `g` on a `BranchHeldLive` `WorktreeProgress` modal (issue #787):
    /// jump to the live session already holding the branch instead of
    /// dead-ending. The holder path is named verbatim in the failure text;
    /// match it against each workspace's session worktree paths (the client
    /// holds the full workspace snapshot) and reveal that workspace.
    pub(super) fn jump_to_worktree_holder(&mut self) {
        let Some(holder) = self
            .worktree_progress
            .as_ref()
            .and_then(|state| state.error())
            .and_then(lazybox_ipc::WorktreeRecovery::holder_path)
        else {
            self.flash_hint("no holder to jump to");
            return;
        };
        let holder_path = std::path::PathBuf::from(&holder);
        let target = self.sidebar.workspaces_iter().find_map(|ws| {
            ws.sessions
                .iter()
                .any(|session| session.worktree_path == holder_path)
                .then(|| lazybox_core::SessionKey::new(ws.key.as_str()))
        });
        match target {
            Some(key) => {
                self.force_dismiss_worktree_progress();
                self.worktree_progress_dismissed = None;
                self.jump_to_workspace_key(&key);
            }
            // No managed session owns the checkout — it's an external
            // worktree (the hint's "or free the external checkout" case).
            // Name it and leave the modal up so the user can act on it.
            None => self.flash_info(format!(
                "no lazybox session holds {holder} — free that external checkout, then retry"
            )),
        }
    }

    /// `r` on a `LinearUnmapped` `WorktreeProgress` modal (#1041): open a
    /// picker of tracked GitHub repos for the ticket's team. Reached only as
    /// the last-resort recovery — the picker normally opens *directly* from
    /// `route_spawn_failure_to_recovery` before any failure modal, so `w w`
    /// never dead-ends. With no team parseable from the error, or no GitHub
    /// repos to offer, it falls back to the manual hint.
    pub(super) fn pick_repo_for_linear_team(&mut self) {
        let Some(team) = self
            .worktree_progress
            .as_ref()
            .and_then(|state| state.error())
            .and_then(lazybox_ipc::WorktreeRecovery::linear_team)
        else {
            self.flash_hint("couldn't read the team — set providers.linear.teams by hand");
            return;
        };
        if !self.mount_linear_team_repo_picker(&team) {
            self.flash_info(format!(
                "no GitHub repos tracked yet — set providers.linear.teams.{team} by hand"
            ));
        }
    }

    /// An unmapped Linear team spawn failure (#1041): open the repo picker
    /// **directly** as the primary path, in place of the "× spawn aborted"
    /// failure modal. Returns `true` when the picker mounted (team parseable
    /// and at least one tracked repo to propose); `false` lets the caller
    /// fall back to the failure modal — the genuine last resort when there
    /// is no repo to offer at all.
    pub(super) fn open_linear_team_repo_picker(&mut self, message: &str) -> bool {
        // One failed provision surfaces twice — a `WorktreeProgress::Failed`
        // step *and* a `spawn:worktree` provider error — and both route
        // here. The first opens the picker; the second must be a no-op, not
        // a second stacked picker.
        if self.modal_stack.contains(&Id::LinearTeamRepo) {
            return true;
        }
        let Some(team) = lazybox_ipc::WorktreeRecovery::linear_team(message) else {
            return false;
        };
        // Never fabricated failure state: a spinner from an earlier progress
        // step is torn down so the picker — not a stuck checklist — is what
        // the user sees.
        self.force_dismiss_worktree_progress();
        self.worktree_progress_dismissed = None;
        self.mount_linear_team_repo_picker(&team)
    }

    /// Mount the team→repo `Choice` picker for `team`, ranking repos the
    /// team's other tickets already link to first (#1041). Returns `false`
    /// without mounting when no GitHub repo is tracked yet — a blank picker
    /// helps no one.
    fn mount_linear_team_repo_picker(&mut self, team: &str) -> bool {
        use crate::realm::components::choice::Choice;

        let repos = self.sidebar.github_repos_ranked_for_linear_team(team);
        if repos.is_empty() {
            return false;
        }
        self.set_modal_flow(ModalFlow::LinearTeamRepo {
            team: team.to_string(),
        });
        let modal = Choice::single(
            format!("Which repo should Linear team {team} use? (saved for its future tickets)"),
            repos,
        )
        .title("Map Linear team")
        .label(|repo: &String| repo.clone())
        .payload_for(|repo: &String| ChoicePayload::Text(repo.clone()));
        self.mount_modal(Id::LinearTeamRepo, modal);
        true
    }

    /// Push a modal.
    pub fn push_modal(&mut self, id: Id) {
        self.modal_stack.push(id.clone());
        let _ = self.app.active(&id);
        self.redraw = true;
    }

    pub(super) fn pop_modal(&mut self) {
        if let Some(top) = self.modal_stack.pop() {
            // Always unmount — every modal id is now transient
            // (mounted on demand by start_setup_wizard / mount_help /
            // mount_reply / etc.).
            let _ = self.app.umount(&top);
        }
        if let Some(top) = self.modal_stack.last() {
            let _ = self.app.active(top);
        }
        // A sidebar `]]` picker's retarget is scoped to that one picker
        // (#871); once it closes, the next snippet/skill/history picker
        // resolves fresh, falling back to the focused terminal.
        self.leader_target = None;
        self.redraw = true;
    }
}

/// `owner/repo` for a `github:owner/repo#N` workspace key, or `None`
/// for a non-GitHub / malformed key (the browser new-issue form is
/// GitHub-specific).
pub(super) fn repo_from_workspace_key(key: &str) -> Option<String> {
    let rest = key.strip_prefix("github:")?;
    let repo = rest.split('#').next()?;
    (repo.matches('/').count() == 1 && !repo.is_empty()).then(|| repo.to_string())
}

/// Truncate to `max` chars, appending `…` when clipped.
pub(super) fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max.saturating_sub(1)).collect();
    out.push('…');
    out
}

/// Minimal percent-encoding for a URL query value — encode everything
/// outside the RFC-3986 unreserved set. Hand-rolled to avoid a new
/// dependency for one call site.
pub(super) fn percent_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
            out.push(b as char);
        } else {
            out.push('%');
            out.push_str(&format!("{b:02X}"));
        }
    }
    out
}

/// Pre-filled GitHub issue body for an error class.
///
/// `message` / `raw` are bounded before they go in: this body is
/// percent-encoded into a `github.com/…/issues/new?…` GET URL, and a
/// multi-KB `raw` (a full GraphQL error, a stack trace) inflates ~3×
/// under encoding and blows past browser / server URL caps — the draft
/// then 414s or silently truncates. Clipping the two unbounded fields
/// keeps the whole URL comfortably viable; the full text still lives in
/// the inbox and the JSONL export.
fn error_issue_body(r: &lazybox_ipc::ErrorInboxRecord) -> String {
    // Char budgets sized so the encoded URL stays well under the ~8 KB
    // ceiling common servers (GitHub included) enforce.
    const MESSAGE_BUDGET: usize = 300;
    const RAW_BUDGET: usize = 1500;
    let mut body = format!(
        "Recurring error surfaced by the lazybox Error Inbox (#831).\n\n\
         - **Class:** `{}`\n\
         - **Source / severity:** {} / {}\n\
         - **Count:** ×{}\n",
        r.dedupe_key, r.source, r.severity, r.count,
    );
    if let Some(op) = &r.operation {
        body.push_str(&format!("- **Operation:** {op}\n"));
    }
    if let Some(ws) = &r.workspace_key {
        body.push_str(&format!("- **Workspace:** {ws}\n"));
    }
    body.push_str(&format!(
        "\n**Sample message:**\n\n> {}\n\n**Raw:**\n\n```\n{}\n```\n",
        truncate(&r.message, MESSAGE_BUDGET),
        truncate(&r.raw, RAW_BUDGET),
    ));
    body
}

/// Opening brief handed to a routed agent for an error class.
fn error_agent_prompt(r: &lazybox_ipc::ErrorInboxRecord) -> String {
    format!(
        "This error class has fired ×{} in lazybox and needs fixing.\n\
         Class: {}\nSource/severity: {} / {}\nSample: {}\nRaw: {}\n\n\
         Find the root cause and open a PR with the fix.",
        r.count, r.dedupe_key, r.source, r.severity, r.message, r.raw,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repo_from_workspace_key_parses_github_only() {
        assert_eq!(
            repo_from_workspace_key("github:o/r#1").as_deref(),
            Some("o/r")
        );
        assert_eq!(
            repo_from_workspace_key("github:owner/repo#42").as_deref(),
            Some("owner/repo")
        );
        // Non-GitHub or malformed keys have no derivable repo.
        assert_eq!(repo_from_workspace_key("linear:TEAM-1"), None);
        assert_eq!(repo_from_workspace_key("github:no-slash#1"), None);
    }

    #[test]
    fn percent_encode_escapes_reserved_and_keeps_unreserved() {
        assert_eq!(percent_encode("a b/c?d"), "a%20b%2Fc%3Fd");
        assert_eq!(percent_encode("A-Z_0.9~"), "A-Z_0.9~");
    }

    #[test]
    fn truncate_appends_ellipsis_only_when_clipped() {
        assert_eq!(truncate("short", 10), "short");
        assert_eq!(truncate("abcdef", 4), "abc…");
    }

    /// A pathologically large `raw` (or `message`) must not bloat the
    /// issue-draft body: it feeds a GET URL, so the fields are clipped.
    /// Without the bound the encoded URL 414s / truncates on GitHub.
    #[test]
    fn error_issue_body_bounds_message_and_raw() {
        let record = lazybox_ipc::ErrorInboxRecord {
            dedupe_key: "github|merge|boom".into(),
            source: "github".into(),
            severity: "permanent".into(),
            operation: Some("merge".into()),
            workspace_key: Some("github:o/r#1".into()),
            message: "m".repeat(5_000),
            raw: "r".repeat(50_000),
            count: 3,
            first_seen: chrono::Utc::now(),
            last_seen: chrono::Utc::now(),
        };
        let body = error_issue_body(&record);
        // The 50 KB raw is clipped to its budget (+ellipsis), not embedded
        // whole; the whole body stays small enough for a GET URL.
        assert!(body.contains('…'), "clipped fields end with an ellipsis");
        assert!(
            !body.contains(&"r".repeat(2_000)),
            "the full 50KB raw must not be embedded"
        );
        assert!(
            body.chars().count() < 2_500,
            "body stays URL-safe, was {} chars",
            body.chars().count()
        );
    }

    /// A minimal open, author, CI-failing PR workspace with the given
    /// labels and CI-auto-fix arm — enough to exercise `build_policy_rows`
    /// glyph/detail logic.
    fn pr_workspace(labels: &[&str], ci_arm: lazybox_core::PolicyArm) -> lazybox_core::Workspace {
        let task = lazybox_core::Task {
            author: String::new(),
            id: lazybox_core::TaskId {
                source: "github".into(),
                key: "owner/repo#1".into(),
            },
            title: "t".into(),
            body: None,
            state: lazybox_core::TaskState::Open,
            role: lazybox_core::TaskRole::Author,
            ci: lazybox_core::CiStatus::Failure,
            review: lazybox_core::ReviewStatus::None,
            checks: vec![],
            unread_count: 0,
            url: "https://github.com/owner/repo/pull/1".into(),
            repo: Some("owner/repo".into()),
            branch: Some("feat".into()),
            base_branch: None,
            updated_at: chrono::DateTime::<chrono::Utc>::from_timestamp(0, 0).unwrap(),
            created_at: None,
            closed_at: None,
            labels: labels
                .iter()
                .map(|l| lazybox_core::Label::new(*l))
                .collect(),
            reviewers: vec![],
            reviews: vec![],
            assignees: vec![],
            auto_merge_enabled: false,
            is_in_merge_queue: false,
            mergeable: lazybox_core::Mergeable::Mergeable,
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
            kind: Some(lazybox_core::TaskKind::Pr),
            closes_issues: vec![],
            linked_tasks: vec![],
            priority: None,
            state_label: None,
        };
        let mut ws = lazybox_core::Workspace::from_task(
            task,
            chrono::DateTime::<chrono::Utc>::from_timestamp(0, 0).unwrap(),
        );
        ws.policies
            .set(lazybox_core::AutoFixKind::CiFailure, ci_arm);
        ws
    }

    /// The CI-auto-fix row of the policies menu (`●` armed / `○` off).
    fn ci_auto_fix_row(labels: &[&str], arm: lazybox_core::PolicyArm, enabled: bool) -> String {
        let (rows, _) =
            build_policy_rows(&pr_workspace(labels, arm), enabled, &["no-auto-fix".into()]);
        // Rows: [merge-on-green, github-auto-merge, auto-fix CI, auto-fix conflict].
        rows[2].clone()
    }

    /// An explicitly-armed workspace still reads as **off** while the
    /// feature is globally disabled — the menu gates through the same
    /// `auto_fix_enabled_and_permitted` composition the daemon uses, so
    /// the glyph can't claim a fix would fire when it wouldn't (tracker
    /// #512).
    #[test]
    fn armed_row_reads_off_when_globally_disabled() {
        let armed_off = ci_auto_fix_row(&[], lazybox_core::PolicyArm::Arm, false);
        assert!(armed_off.starts_with('○'), "globally-off: {armed_off:?}");
        assert!(
            armed_off.contains("disabled globally"),
            "detail must explain the global-off: {armed_off:?}"
        );
        // …and the same arm reads on once the feature is enabled.
        let armed_on = ci_auto_fix_row(&[], lazybox_core::PolicyArm::Arm, true);
        assert!(armed_on.starts_with('●'), "globally-on: {armed_on:?}");
    }

    /// With the feature enabled, the row tracks the arm × label rule:
    /// `Arm` overrides an opt-out label (on), `Default` follows it (off),
    /// `Disarm` is always off.
    #[test]
    fn enabled_row_tracks_arm_and_label() {
        assert!(ci_auto_fix_row(&[], lazybox_core::PolicyArm::Default, true).starts_with('●'));
        assert!(
            ci_auto_fix_row(&["no-auto-fix"], lazybox_core::PolicyArm::Default, true)
                .starts_with('○')
        );
        assert!(
            ci_auto_fix_row(&["no-auto-fix"], lazybox_core::PolicyArm::Arm, true).starts_with('●')
        );
        assert!(ci_auto_fix_row(&[], lazybox_core::PolicyArm::Disarm, true).starts_with('○'));
    }

    /// #794: the two merge-on-green rows must state, in words, *who*
    /// merges and whether it survives closing lazybox — the ` ARM ` /
    /// ` AUTO ` pills look alike, so the decision surface has to spell out
    /// the durability difference.
    #[test]
    fn merge_rows_spell_out_durability() {
        let (rows, _) = build_policy_rows(
            &pr_workspace(&[], lazybox_core::PolicyArm::Default),
            true,
            &["no-auto-fix".into()],
        );
        // Row 0: lazybox client-side merge-on-green.
        assert!(rows[0].contains("merge on green"), "{:?}", rows[0]);
        assert!(
            rows[0].contains("lazybox") && rows[0].contains("only while lazybox runs"),
            "merge-on-green row must name lazybox and its while-running limit: {:?}",
            rows[0]
        );
        // Row 1: GitHub-native, durable.
        assert!(rows[1].contains("GitHub auto-merge"), "{:?}", rows[1]);
        assert!(
            rows[1].contains("even when lazybox is closed"),
            "GitHub auto-merge row must name its offline durability: {:?}",
            rows[1]
        );
    }

    fn dto_with(
        reasons: &[&str],
        dirty: bool,
        unpushed: bool,
        size: u64,
    ) -> lazybox_ipc::WorktreeInspectionDto {
        lazybox_ipc::WorktreeInspectionDto {
            path: std::path::PathBuf::from("/tmp/worktrees/o-r-feat"),
            bare_path: None,
            branch: Some("feat".into()),
            session_id: None,
            reasons: reasons.iter().map(|s| s.to_string()).collect(),
            size_bytes: size,
            // Fixed Unix epoch so the age field is deterministic
            // relative to wall-clock at test time.
            last_modified_unix: Some(0),
            has_uncommitted_changes: dirty,
            has_unpushed_commits: unpushed,
            is_safe_to_delete: false,
        }
    }

    fn merged_prompt(terminal_count: usize, has_local_work: bool) -> super::super::RemovalPrompt {
        super::super::RemovalPrompt {
            workspace_key: lazybox_core::WorkspaceKey::new("github:o/r#7"),
            label: "o/r#7".into(),
            title: None,
            terminal_count,
            reason: super::super::RemovalReason::Merged,
            has_local_work,
        }
    }

    /// Clean merge with no live terminals → a plain ask, no warning.
    #[test]
    fn merged_copy_clean_has_no_warning() {
        let copy = terminal_removal_copy(&merged_prompt(0, false), "merged");
        assert_eq!(
            copy,
            "o/r#7 was merged — remove workspace and delete its worktree?"
        );
    }

    /// The closed-issue path shares the copy builder but names the
    /// "closed" verb so the modal reads correctly for issues.
    #[test]
    fn closed_copy_names_closed_verb() {
        let copy = terminal_removal_copy(&merged_prompt(0, false), "closed");
        assert_eq!(
            copy,
            "o/r#7 was closed — remove workspace and delete its worktree?"
        );
    }

    /// Live terminals + local work → both are named in a single
    /// "will be lost" warning so the user knows what `yes` destroys.
    #[test]
    fn merged_copy_warns_about_terminals_and_local_work() {
        let copy = terminal_removal_copy(&merged_prompt(2, true), "merged");
        assert!(copy.contains("2 running terminals"), "got: {copy}");
        assert!(copy.contains("uncommitted or unpushed work"), "got: {copy}");
        assert!(copy.contains("will be lost"), "got: {copy}");
    }

    /// Issue #314: the issue→PR session-move prompt says "join" —
    /// matching the `x j` action and the flash — never "merge",
    /// which collides with the nearby `g m` git-merge action and reads
    /// like a PR merge.
    #[test]
    fn merge_prompt_says_join_not_merge() {
        let one = merge_prompt_question("o/r#2", "o/r#1", 1);
        assert!(one.contains("Join the issue's sessions"), "got: {one}");
        assert!(!one.to_lowercase().contains("merge"), "got: {one}");
        assert!(one.contains("1 running terminal"), "got: {one}");

        let many = merge_prompt_question("o/r#2", "o/r#1", 3);
        assert!(many.contains("3 running terminals"), "got: {many}");
        assert!(!many.to_lowercase().contains("merge"), "got: {many}");
    }

    /// The bulk-shortcut row renders as a single distinctive line so
    /// users spot it instantly at the top of the picker.
    #[test]
    fn bulk_safe_row_label() {
        let label = format_inspect_row(&InspectRow::BulkSafe { count: 7 });
        assert_eq!(label, "▶ Delete all 7 clearly-safe worktrees");
    }

    /// Healthy worktree → "[ok] name · branch · size · age" with no
    /// status flags.
    #[test]
    fn healthy_row_label_uses_ok_tag() {
        let dto = dto_with(&[], false, false, 2048);
        let label = format_inspect_row(&InspectRow::Inspection(dto));
        // age depends on `now`, so only assert the stable prefix.
        assert!(label.starts_with("[ok] o-r-feat · feat · 2.0K · "));
        assert!(!label.contains("DIRTY"));
        assert!(!label.contains("UNPUSHED"));
    }

    /// Multi-reason orphan: tags joined with comma, no spaces, in
    /// the order the inspector pushed them.
    #[test]
    fn multi_reason_label_joins_with_comma() {
        let dto = dto_with(&["untracked", "branch-deleted-upstream"], false, false, 0);
        let label = format_inspect_row(&InspectRow::Inspection(dto));
        assert!(
            label.starts_with("[untracked,branch-deleted-upstream] o-r-feat ·"),
            "got: {label}"
        );
    }

    /// Dirty + unpushed row carries both flags in a single trailing
    /// bracket so the user sees the "needs FORCE" signal at a glance.
    #[test]
    fn dirty_and_unpushed_show_both_flags() {
        let dto = dto_with(&["untracked"], true, true, 0);
        let label = format_inspect_row(&InspectRow::Inspection(dto));
        assert!(label.ends_with(" [DIRTY,UNPUSHED]"), "got: {label}");
    }

    /// Only-dirty and only-unpushed each render exactly one flag,
    /// not both.
    #[test]
    fn single_flag_rows_render_one_flag() {
        let dirty_only = dto_with(&["untracked"], true, false, 0);
        let unpushed_only = dto_with(&["untracked"], false, true, 0);
        assert!(format_inspect_row(&InspectRow::Inspection(dirty_only)).ends_with(" [DIRTY]"));
        assert!(
            format_inspect_row(&InspectRow::Inspection(unpushed_only)).ends_with(" [UNPUSHED]")
        );
    }

    /// Size formatter scales across the unit boundaries the user
    /// will actually see in the wild — a healthy worktree of ~200MB
    /// (one cargo target), a fresh checkout (~kilobytes), and a
    /// neglected one in gigabytes.
    #[test]
    fn size_formatter_picks_units() {
        assert_eq!(format_size(0), "0B");
        assert_eq!(format_size(512), "512B");
        assert_eq!(format_size(2 * 1024), "2.0K");
        assert_eq!(format_size(200 * 1024 * 1024), "200.0M");
        assert_eq!(format_size(3 * 1024 * 1024 * 1024), "3.0G");
    }

    /// `None` mtime is the "vanished dir" case — render an em-dash
    /// so the column lines up with real ages.
    #[test]
    fn age_formatter_handles_missing_mtime() {
        assert_eq!(format_age_short(None), "—");
    }
}
