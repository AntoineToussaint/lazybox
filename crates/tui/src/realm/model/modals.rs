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

impl<T: TerminalAdapter> Model<T> {
    /// Mount the reply textarea targeted at `workspace_key`. Submit
    /// → `Msg::TextareaSubmitted(body)` → orchestrator builds a
    /// `Command::PostReply { session_key, body }`.
    pub(super) fn mount_reply(&mut self, workspace_key: pilot_core::SessionKey) {
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
    pub(super) fn mount_new_workspace_input(&mut self, project_key: pilot_core::ProjectKey) {
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
    pub(crate) fn mount_request_reviewers(&mut self, workspace_key: pilot_core::WorkspaceKey) {
        use crate::realm::components::choice::Choice;

        if matches!(self.modal_stack.last(), Some(Id::RequestReviewers)) {
            return;
        }
        let candidates = self.gather_candidate_logins(&workspace_key, true);
        if candidates.is_empty() {
            self.flash_info("no candidate reviewers yet — interact with the PR first");
            return;
        }
        let labels: Vec<String> = candidates.iter().map(|l| format!("@{l}")).collect();
        self.review_choices = candidates;
        self.pending_review_request = Some(workspace_key);
        let modal = Choice::multi("Request review from", labels)
            .title("Add reviewers")
            .label(|s: &String| s.clone());
        self.mount_modal(Id::RequestReviewers, modal);
    }

    /// Mount the "assignees" multi-select picker for the workspace's
    /// PR or issue. Pre-checks the currently-assigned logins so this
    /// is a "change assignees" UX (toggle to add / untoggle to
    /// remove) rather than an additive picker — submitting fires
    /// `Command::SetAssignees`, which diffs against the persisted
    /// task and runs add + remove mutations as needed.
    pub(crate) fn mount_add_assignees(&mut self, workspace_key: pilot_core::WorkspaceKey) {
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

    /// Mount the snooze duration picker. Used by `z` (ToggleSnooze)
    /// when the workspace is NOT currently snoozed — the user picks
    /// the duration instead of always paying the YAML default.
    /// Cycle of options is curated: each one's a "I'll come back
    /// to this when…" moment that maps to a real schedule.
    pub(crate) fn mount_snooze_picker(&mut self, session_key: pilot_core::SessionKey) {
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
        workspace_key: &pilot_core::WorkspaceKey,
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
        workspace_key: &pilot_core::WorkspaceKey,
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
        self.mount_modal(Id::Help, Help::from_catalog());
    }

    /// If there's a queued "out-of-scope workspace has active
    /// sessions" prompt and no modal is currently up, mount it. The
    /// user's answer (Y → kill, N/Esc → keep) is handled in the
    /// `Msg::Confirmed` / `Msg::ModalDismissed` arms.
    pub(super) fn maybe_mount_next_removal_prompt(&mut self) {
        use crate::realm::components::confirm::Confirm;

        if !self.modal_stack.is_empty() {
            return;
        }
        let Some((workspace_key, label, title, count)) = self.pending_removal_prompts.pop_front()
        else {
            return;
        };
        let terminals_phrase = if count == 1 {
            "1 running terminal".to_string()
        } else {
            format!("{count} running terminals")
        };
        // Trim the title so a verbose PR description doesn't make the
        // modal three lines tall. 80 chars + an ellipsis fits within
        // the dynamic-height Confirm modal cleanly.
        let runner_label = match title.as_deref().filter(|s| !s.is_empty()) {
            Some(t) => {
                let title_short = if t.chars().count() > 80 {
                    let truncated: String = t.chars().take(79).collect();
                    format!("{truncated}…")
                } else {
                    t.to_string()
                };
                format!(
                    "{label} \"{title_short}\" is no longer in your filter scope but has {terminals_phrase} — kill and remove?"
                )
            }
            None => format!(
                "{label} is no longer in your filter scope but has {terminals_phrase} — kill and remove?"
            ),
        };
        let modal = Confirm::new(runner_label).default_no();
        self.active_removal_prompt = Some(workspace_key);
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
        action: pilot_tui_core::action::Action,
        override_prompt: Option<String>,
    ) {
        use crate::realm::components::confirm::Confirm;
        use pilot_tui_core::action::ActionDef;
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
        self.pending_action_confirm = Some(action);
        let modal = Confirm::new(&prompt).default_no();
        self.mount_modal(Id::ActionConfirm, modal);
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
    pub(super) fn mount_sidebar_context_menu(&mut self, session_key: pilot_core::SessionKey) {
        use crate::realm::components::choice::Choice;
        use pilot_tui_core::action::{Action, ActionDef, ActionKind, availability};

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
                // The catalog can't know about pilot's setup state,
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

    pub(super) fn mount_adopt_picker(&mut self, source_key: pilot_core::WorkspaceKey) {
        use crate::realm::components::choice::Choice;

        // Build (target_key, label) pairs from every workspace EXCEPT
        // the source. Labels prefer the primary task's `owner/repo#N`
        // form so the picker reads like the inbox rows.
        let mut items: Vec<(pilot_core::WorkspaceKey, String)> = Vec::new();
        for (key, ws) in self.sidebar.workspace_iter() {
            if key.as_str() == source_key.as_str() {
                continue;
            }
            let label = ws
                .primary_task()
                .map(|t| t.id.key.clone())
                .unwrap_or_else(|| ws.name.clone());
            items.push((pilot_core::WorkspaceKey::new(key.as_str()), label));
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
