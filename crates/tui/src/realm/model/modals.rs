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

use super::{Id, Model};
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

/// Tabular label for one inspector row. Single-line — the Choice
/// modal truncates with an ellipsis when it overflows. Pack the
/// signal-dense bits first: name, reasons, size, age, flags.
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

/// Confirm copy for a merged PR. Always names the worktree deletion;
/// appends a warning when there are live terminals or local
/// (uncommitted/unpushed) work, since "yes" force-deletes regardless.
fn merged_removal_copy(prompt: &super::RemovalPrompt) -> String {
    let label = &prompt.label;
    let mut warnings: Vec<String> = Vec::new();
    if prompt.terminal_count > 0 {
        warnings.push(terminals_phrase(prompt.terminal_count));
    }
    if prompt.has_local_work {
        warnings.push("uncommitted or unpushed work".to_string());
    }
    if warnings.is_empty() {
        format!("{label} was merged — remove workspace and delete its worktree?")
    } else {
        format!(
            "{label} was merged — remove workspace and delete its worktree? \
             Warning: it has {} that will be lost.",
            warnings.join(" and "),
        )
    }
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
        self.pending_reply = Some(workspace_key);
        self.mount_modal(Id::Reply, modal);
    }

    /// Mount the "New workspace" name prompt under a specific
    /// Project. Submit → `Msg::InputSubmitted(name)` while
    /// `Id::NewWorkspace` is on top → `Command::CreateWorkspace
    /// { name, project_key }`. The project_key is stashed on self
    /// here and consumed by `handle_input_submitted`.
    pub(super) fn mount_new_workspace_input(&mut self, project_key: lazybox_core::ProjectKey) {
        use crate::realm::components::input::Input;

        if matches!(self.modal_stack.last(), Some(Id::NewWorkspace)) {
            return;
        }
        self.pending_new_workspace_project = Some(project_key);

        let modal = Input::new("Name this workspace")
            .title("New workspace")
            .placeholder("e.g. spike-rate-limit, refactor-auth, …")
            .with_validator(|s: &str| !s.trim().is_empty());
        self.mount_modal(Id::NewWorkspace, modal);
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

    /// Mount the "request reviewers" multi-select picker for the
    /// given workspace's PR. Candidates are gathered from the
    /// workspace's known people; Space toggles, Enter submits →
    /// `Msg::ChoicePicked(indices)` → `handle_choice_picked` looks
    /// up the chosen logins in `review_choices` and dispatches
    /// `Command::RequestReviewers`.
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
            let labels: Vec<String> = candidates.iter().map(|l| format!("@{l}")).collect();
            self.review_choices = candidates;
            Choice::multi("Request review from", labels)
                .title("Add reviewers")
                .label(|s: &String| s.clone())
        };
        self.pending_review_request = Some(workspace_key);
        self.mount_modal(Id::RequestReviewers, modal);
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
        let labels: Vec<String> = candidates.iter().map(|l| format!("@{l}")).collect();
        self.assignees_choices = candidates.clone();
        self.pending_assignees_request = Some(workspace_key);
        let modal = Choice::multi("Assign to", labels)
            .title("Assignees (toggle to add/remove)")
            .label(|s: &String| s.clone())
            .with_selected_by(move |label: &String| {
                let login = label.strip_prefix('@').unwrap_or(label);
                existing.contains(login)
            });
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

        // Don't trample whatever's currently on top.
        if matches!(self.modal_stack.last(), Some(Id::ManageLabels)) {
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
        // Each row reads `[name]` so the user gets the same chip
        // framing the sidebar uses — keeps mental model consistent.
        // `labels_choices` stays as the bare names (what the picker
        // submits back upstream); the bracketed form only lives in
        // the picker's display labels.
        let labels_for_picker: Vec<String> = repo_labels
            .iter()
            .map(|l| format!("[{}]", l.name))
            .collect();
        self.labels_choices = repo_labels.into_iter().map(|l| l.name).collect();
        self.pending_labels_request = Some(workspace_key);
        let modal = Choice::multi("Apply labels", labels_for_picker)
            .title("Labels (toggle to add/remove)")
            .label(|s: &String| s.clone())
            .with_selected_by(move |label: &String| {
                let trimmed = label
                    .strip_prefix('[')
                    .and_then(|s| s.strip_suffix(']'))
                    .unwrap_or(label);
                existing.contains(trimmed)
            });
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
        let labels: Vec<String> = options.iter().map(|(l, _)| (*l).to_string()).collect();
        self.snooze_choices = options.into_iter().map(|(_, d)| d).collect();
        self.pending_snooze_workspace = Some(session_key);
        let modal = Choice::single("Snooze for…", labels)
            .title("Snooze duration")
            .label(|s: &String| s.clone());
        self.mount_modal(Id::SnoozeDuration, modal);
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
        self.mount_modal(Id::Help, Help::from_catalog(&self.action_key_overrides));
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
            SyncStatus::new(summary, recent, chrono::Utc::now()),
        );
    }

    /// If there's a queued workspace-removal prompt and no modal is
    /// currently up, mount it. Copy depends on the prompt's
    /// [`RemovalReason`]; the user's answer (Y → remove, N/Esc → keep)
    /// is handled in the `Msg::Confirmed` / `Msg::ModalDismissed` arms.
    pub(super) fn maybe_mount_next_removal_prompt(&mut self) {
        use super::RemovalReason;
        use crate::realm::components::confirm::Confirm;

        if !self.modal_stack.is_empty() {
            return;
        }
        let Some(prompt) = self.pending_removal_prompts.pop_front() else {
            return;
        };
        let copy = match prompt.reason {
            RemovalReason::OutOfScope => out_of_scope_copy(&prompt),
            RemovalReason::Merged => merged_removal_copy(&prompt),
        };
        let modal = Confirm::new(copy).default_no();
        self.active_removal_prompt = Some((prompt.workspace_key, prompt.reason));
        self.mount_modal(Id::RemoveOutOfScope, modal);
    }

    /// Mount the `Shift-A` adopt-target picker. Lists every other
    /// workspace the user could move sessions into. No-op when there
    /// are no other workspaces — show a hint instead since there's
    /// nothing to pick.
    /// Unified Confirm-modal mount for any destructive catalog
    /// action. Stashes the action in `pending_action_confirm`;
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
        target: super::ActionConfirmTarget,
        override_prompt: Option<String>,
    ) {
        use crate::realm::components::confirm::Confirm;
        use lazybox_tui_core::action::ActionDef;
        // Override wins so callers can render context-sensitive copy
        // (e.g. "Delete project X with 3 workspaces" vs. the generic
        // "Archive the focused workspace"). Catalog default is the
        // safety net when no override is available.
        let prompt: String = override_prompt.unwrap_or_else(|| {
            ActionDef::for_action(&action)
                .confirm_prompt()
                .unwrap_or("Confirm action?")
                .to_string()
        });
        self.pending_action_confirm = Some((action, target));
        let modal = Confirm::new(&prompt).default_no();
        self.mount_modal(Id::ActionConfirm, modal);
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
    /// `pending_inspect_rows` so the choice handler can index back.
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
        // back to inspections (skipping the sentinel as needed).
        self.pending_inspect_rows = rows;

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
        let modal = Confirm::new(&prompt).default_no();
        self.pending_inspect_target = Some(target);
        self.mount_modal(Id::InspectConfirm, modal);
    }

    /// Confirm prompt before dispatching `Command::CleanWorktrees`.
    /// The destructive bit is on disk — sessions + their worktrees
    /// are gone after this. PR/issue rows stay because we only
    /// touch session records. `Msg::Confirmed(true)` fires the IPC;
    /// `(false)` / dismiss drops the prompt silently.
    pub(super) fn mount_clean_worktrees_confirm(&mut self) {
        use crate::realm::components::confirm::Confirm;
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

        self.pending_sidebar_context = Some((session_key, actions));
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
        let labels: Vec<String> = items.iter().map(|(_, l)| l.clone()).collect();
        self.adopt_choices = items.into_iter().map(|(k, _)| k).collect();
        self.pending_adopt_source = Some(source_key);

        let modal = Choice::single("Move sessions to which workspace?", labels)
            .title("Adopt sessions")
            .label(|s: &String| s.clone());
        self.mount_modal(Id::AdoptTarget, modal);
    }

    /// Surface the next queued issue→PR merge prompt when no modal
    /// is currently up. The user's answer drives `Msg::Confirmed` /
    /// `Msg::ModalDismissed`, which dispatch a `Command::ConfirmMerge`
    /// back to the daemon. Default-no: silently absorbing a session
    /// the user is in the middle of using would be the surprising
    /// outcome, so Enter biases toward "leave them separate".
    pub(super) fn maybe_mount_next_merge_prompt(&mut self) {
        use crate::realm::components::confirm::Confirm;

        if !self.modal_stack.is_empty() {
            return;
        }
        let Some((issue_key, pr_key, issue_label, pr_label, count)) =
            self.pending_merge_prompts.pop_front()
        else {
            return;
        };
        let terminals_phrase = if count == 1 {
            "1 running terminal".to_string()
        } else {
            format!("{count} running terminals")
        };
        let question = format!(
            "{pr_label} closes {issue_label}, which has {terminals_phrase}. \
             Merge the issue's sessions into the PR workspace?",
        );
        let modal = Confirm::new(question).default_no();
        self.active_merge_prompt = Some((issue_key, pr_key));
        self.mount_modal(Id::MergeConfirm, modal);
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
        self.redraw = true;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
        let copy = merged_removal_copy(&merged_prompt(0, false));
        assert_eq!(
            copy,
            "o/r#7 was merged — remove workspace and delete its worktree?"
        );
    }

    /// Live terminals + local work → both are named in a single
    /// "will be lost" warning so the user knows what `yes` destroys.
    #[test]
    fn merged_copy_warns_about_terminals_and_local_work() {
        let copy = merged_removal_copy(&merged_prompt(2, true));
        assert!(copy.contains("2 running terminals"), "got: {copy}");
        assert!(copy.contains("uncommitted or unpushed work"), "got: {copy}");
        assert!(copy.contains("will be lost"), "got: {copy}");
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
