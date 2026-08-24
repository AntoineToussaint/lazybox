//! Catalog-action dispatch: `dispatch_action` (destructive-gate
//! wrapper) and `dispatch_action_unchecked` (the actual fan-out).
//! Every catalog `Action` whose effect fits a single IpcCommand or
//! modal mount lands here. The keyboard, right-click menu, palette,
//! and future remap UI all funnel through `dispatch_action` so
//! behavior stays consistent across surfaces.

use super::{ActionConfirmTarget, Model, PaneFocus};
use lazybox_ipc::Command as IpcCommand;
use tuirealm::terminal::TerminalAdapter;

/// The ` #N` suffix parsed from a task id key
/// (`"github:o/r#42"` → `" #42"`), or empty when the key carries no
/// number. Feeds the pending close footer notices. (The
/// *workspace* key won't do — `sanitize_key` strips the `#`.)
fn task_number_suffix(task_key: &str) -> String {
    task_key
        .rsplit_once('#')
        .map(|(_, n)| format!(" #{n}"))
        .unwrap_or_default()
}

/// A resolved workspace operation the unified fan-out applies uniformly
/// to every target (#1077): the action + its payload, decided ONCE, then
/// handed to [`Model::apply_one`] per target. Bulk `v`-selection work
/// (#899) and snippet / free-text broadcast (#836) are all just variants
/// here — the same pipeline resolves targets once, resolves the op once,
/// and applies that same op to each. Tiers ride alongside as a separate
/// `model_alias`.
enum BulkOp {
    /// `w w`: continue the contextual agent per workspace, else spawn it.
    Work,
    /// `w c` / `w x` / `w u`: same, but force a specific agent id.
    WorkWith(String),
    /// `a c` / `a x` / `a u` / `a S`: always spawn a fresh agent.
    SpawnAgent(String),
    /// `r c` / `r x` / `r u`: spawn a fresh agent on the named remote box
    /// (#965) — same fan-out and heavy-spawn confirm as [`Self::SpawnAgent`],
    /// but each spawn routes to the box's client and tags its row `⇅`.
    SpawnAgentRemote(String, String),
    /// `s`: always spawn a shell.
    SpawnShell,
    /// Deliver a snippet to each target (snippet broadcast / sidebar `]]s`
    /// fan-out, #1077). Category + body are resolved once at op-build
    /// time. A live session gets the confirmed-delivery command; a
    /// session-less but spawnable workspace spawns the default agent
    /// seeded with the body (#836); a repo-less row is skipped.
    Snippet {
        key: String,
        category: String,
        body: String,
    },
    /// Deliver a free-text prompt to each target (free-text broadcast,
    /// #836). Same target handling as [`Self::Snippet`], but a live agent
    /// gets the settle-gated inject and a live shell the encoded write.
    Prompt { body: String },
}

/// What applying a [`BulkOp`] to one target yields (#1077) — the return
/// of [`Model::apply_one`]. The fan-out loop folds these into a
/// [`BulkAgentPlan`]: a `Spawn` counts as a heavy new session (and pins
/// the focus-follow), a `Live` delivery injects/writes into a running
/// session, a `Skip` is named in the summary, and `Gone` is dropped
/// silently (the row vanished between mark and fire).
enum ApplyOutcome {
    Spawn {
        step: super::BulkAgentStep,
        follow: lazybox_core::SessionKey,
    },
    Live(super::BulkAgentStep),
    Skip(String),
    Gone,
}

/// The pre-computed result of a bulk agent fan-out: the ordered,
/// side-effect-free steps to run, the spawn/inject/skip tallies driving
/// the confirm copy and summary, and the workspace focus should follow
/// to. The steps stay inert (no recap mutation) until run.
#[derive(Default)]
struct BulkAgentPlan {
    steps: Vec<super::BulkAgentStep>,
    spawned: usize,
    injected: usize,
    skipped: Vec<String>,
    follow: Option<lazybox_core::SessionKey>,
}

/// Build a bulk spawn `Command` with the shared defaults every bulk
/// start uses (`session_id`/`cwd` unset, `on_main` false).
fn bulk_spawn_command(
    session_key: lazybox_core::SessionKey,
    kind: lazybox_ipc::TerminalKind,
    initial_prompt: Option<String>,
    initial_snippet: Option<lazybox_ipc::SnippetRef>,
    model_alias: Option<String>,
) -> IpcCommand {
    IpcCommand::Spawn {
        session_key,
        session_id: None,
        client_request_id: None,
        kind,
        cwd: None,
        initial_prompt,
        initial_snippet: initial_snippet.map(Box::new),
        on_main: false,
        model_alias,
        access: lazybox_ipc::AgentRunAccess::Default,
        // Bulk starts inject into any workspace already running an agent
        // (#932) — reuse-friendly, so never force a duplicate.
        force_new: false,
    }
}

/// "started N agents · continued M · K skipped (…)" — the outcome
/// notice a bulk agent fan-out flashes once it runs.
fn bulk_agent_summary(plan: &BulkAgentPlan) -> String {
    let plural = |n: usize| if n == 1 { "" } else { "s" };
    let mut parts: Vec<String> = Vec::new();
    if plan.spawned > 0 {
        parts.push(format!(
            "started {} agent{}",
            plan.spawned,
            plural(plan.spawned)
        ));
    }
    if plan.injected > 0 {
        parts.push(format!("continued {}", plan.injected));
    }
    if !plan.skipped.is_empty() {
        parts.push(format!(
            "{} skipped: {}",
            plan.skipped.len(),
            truncate_affected_list(&plan.skipped),
        ));
    }
    if parts.is_empty() {
        "nothing to work on".to_string()
    } else {
        parts.join(" · ")
    }
}

/// "queued for N · started M · K skipped (no repo): …" — the outcome
/// notice a snippet / free-text broadcast (#836) flashes. Distinct copy
/// from [`bulk_agent_summary`] (a broadcast "queues" deliveries and
/// "starts" seeded agents), but the same [`BulkAgentPlan`] tallies.
fn broadcast_summary(plan: &BulkAgentPlan) -> String {
    let plural = |n: usize| if n == 1 { "" } else { "s" };
    let mut parts: Vec<String> = Vec::new();
    if plan.injected > 0 {
        parts.push(format!(
            "queued for {} workspace{}",
            plan.injected,
            plural(plan.injected)
        ));
    }
    if plan.spawned > 0 {
        parts.push(format!(
            "started {} agent{}",
            plan.spawned,
            plural(plan.spawned)
        ));
    }
    if !plan.skipped.is_empty() {
        parts.push(format!(
            "{} skipped (no repo): {}",
            plan.skipped.len(),
            plan.skipped.join(", "),
        ));
    }
    if parts.is_empty() {
        "broadcast reached nobody".to_string()
    } else {
        parts.join(" · ")
    }
}

/// The "start N agents?" confirm copy shown before a bulk fan-out spins
/// up new sessions (#836): names the spawn count plus any live-agent
/// continues and skips, so it isn't a blind "Y".
fn bulk_agent_confirm_prompt(plan: &BulkAgentPlan, remote: Option<&str>) -> String {
    let plural = |n: usize| if n == 1 { "" } else { "s" };
    let mut prompt = format!("Start {} new agent{}", plan.spawned, plural(plan.spawned));
    if let Some(remote) = remote {
        prompt.push_str(&format!(" on box ⇅ {remote}"));
    }
    if plan.injected > 0 {
        prompt.push_str(&format!(" (and continue {} live)", plan.injected));
    }
    if !plan.skipped.is_empty() {
        prompt.push_str(&format!(
            ", skipping {} ({})",
            plan.skipped.len(),
            truncate_affected_list(&plan.skipped),
        ));
    }
    prompt.push('?');
    prompt
}

/// The destructive actions a `v` multi-selection fans out over. Every
/// other destructive action stays focused-only even under a selection
/// (see [`Model::resolve_confirm_targets`]) — so this is also the exact
/// set [`bulk_confirm_prompt`] and [`bulk_confirmed_verb`] must render
/// copy for. Adding an action here without adding its copy there falls
/// through to a neutral prompt, never a wrong one. Delete-or-close and
/// close-issue were wrongly held out of this set — with rows marked,
/// `g d` silently acted on ONE (#1243); selection-first (#932) means a
/// marked set is the target unless the action is truly single-target.
pub(super) fn is_bulk_destructive(action: &lazybox_tui_core::action::Action) -> bool {
    use lazybox_tui_core::action::Action;
    matches!(
        action,
        Action::Archive
            | Action::MergePr
            | Action::LongSnooze
            | Action::DeleteOrClose
            | Action::CloseIssue
            | Action::CloseAndArchive
    )
}

/// Past-tense verb for the bulk-confirmed summary notice, per action.
/// Only the [`is_bulk_destructive`] actions reach this; the neutral
/// fallback is a safety net, not a lie about what happened.
fn bulk_confirmed_verb(action: &lazybox_tui_core::action::Action) -> &'static str {
    use lazybox_tui_core::action::Action;
    match action {
        Action::Archive => "archived",
        Action::MergePr => "merged",
        Action::LongSnooze => "snoozed",
        Action::DeleteOrClose => "closed/deleted",
        Action::CloseIssue => "closed",
        Action::CloseAndArchive => "closed & killed",
        _ => "applied to",
    }
}

/// Render an affected-workspace list for bulk confirm copy: the first
/// few names inline, the remainder collapsed to `+N more` so a large
/// selection can't overflow the modal.
fn truncate_affected_list(names: &[String]) -> String {
    const SHOWN: usize = 5;
    if names.len() <= SHOWN {
        names.join(", ")
    } else {
        let head = names[..SHOWN].join(", ");
        format!("{head}, +{} more", names.len() - SHOWN)
    }
}

/// Uppercase the first letter of composed confirm copy — the verb list
/// is built lowercase so it can be joined mid-sentence.
fn capitalize_first(s: &str) -> String {
    let mut chars = s.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
        None => String::new(),
    }
}

/// Cap a task title for confirm-modal copy (same 80-char convention
/// as the removal prompts) so a long title can't blow up the modal.
fn truncate_title(title: &str) -> String {
    if title.chars().count() > 80 {
        let truncated: String = title.chars().take(79).collect();
        format!("{truncated}…")
    } else {
        title.to_string()
    }
}

impl<T: TerminalAdapter> Model<T> {
    fn execute_dispatch_intent(
        &mut self,
        intent: crate::intent::Intent,
        workspace: Option<&lazybox_core::Workspace>,
    ) -> Vec<IpcCommand> {
        use crate::intent::Intent;

        if let Some(notice) = crate::intent::pending_notice(&intent, workspace) {
            self.flash_info(notice);
        }
        match intent {
            Intent::MergePr { workspace_key } => {
                vec![IpcCommand::MergePr { workspace_key }]
            }
            Intent::UpdateBranch { workspace_key } => {
                vec![IpcCommand::UpdateBranch { workspace_key }]
            }
            Intent::Snooze {
                session_key,
                duration,
            } => {
                let until = chrono::Utc::now()
                    + chrono::Duration::from_std(duration)
                        .unwrap_or_else(|_| chrono::Duration::days(365));
                vec![IpcCommand::Snooze { session_key, until }]
            }
            Intent::MarkAllRead { session_key } => {
                vec![IpcCommand::MarkRead { session_key }]
            }
            Intent::MarkActivitiesRead {
                session_key,
                targets,
                optimistic,
                notice,
            } => {
                if let Some(notice) = notice {
                    self.flash_info(notice);
                }
                if optimistic {
                    for target in &targets {
                        self.right.mark_activity_read_locally(target.index);
                    }
                }
                targets
                    .into_iter()
                    .map(|target| IpcCommand::MarkActivityRead {
                        session_key: session_key.clone(),
                        index: target.index,
                        fingerprint: target.fingerprint,
                    })
                    .collect()
            }
            Intent::CollapseIntoPr {
                issue_workspace_key,
            } => vec![IpcCommand::CollapseIntoPr {
                issue_workspace_key,
            }],
            Intent::MountHandoffPicker {
                source_key,
                source_name,
                seed,
                notice,
            } => {
                if let Some(notice) = notice {
                    self.flash_info(notice);
                }
                self.mount_handoff_picker(&source_key, source_name, seed);
                Vec::new()
            }
            Intent::Notice(notice) => {
                self.flash_info(notice);
                Vec::new()
            }
            Intent::NoOp
            | Intent::SpawnAgent { .. }
            | Intent::SpawnShell { .. }
            | Intent::MountReply { .. }
            | Intent::MountNewWorkspaceInput { .. }
            | Intent::MountAdoptPicker { .. }
            | Intent::OpenEditor
            | Intent::SetAutoMergeOnGreen { .. }
            | Intent::SetTrackMain { .. }
            | Intent::KillWorkspace { .. }
            | Intent::Unsnooze { .. } => Vec::new(),
        }
    }

    /// Kick off the reviewer picker for the focused workspace's PR.
    /// Two-step like the label picker (`g l`): ask the daemon for the
    /// repo's requestable reviewers, then mount the picker when
    /// `Event::RequestableReviewers` arrives (see
    /// [`Model::mount_request_reviewers`]). Returns the fetch command
    /// to enqueue, or `None` when the focused workspace has no PR.
    /// Shared by the `g r` action and the clickable header "Reviewers:"
    /// line.
    pub(crate) fn begin_request_reviewers(&mut self) -> Option<IpcCommand> {
        let ws = self.sidebar.selected_workspace()?;
        // Only PRs have reviewers — bail when the focused workspace has no PR.
        ws.pr.as_ref()?;
        let ws_key = ws.key.clone();
        self.awaiting_requestable_reviewers = Some(ws_key.clone());
        self.flash_hint("loading reviewers…");
        Some(IpcCommand::FetchRequestableReviewers {
            workspace_key: ws_key,
        })
    }

    /// Single fan-out from a catalog `Action` to its effect (IPC
    /// command, modal mount, focus shift, …). Surfaces (keyboard,
    /// right-click menu, future remap UI) all call this so behavior
    /// stays consistent across them.
    ///
    /// **Returns** the IPC commands the action produces, if any.
    /// UI-only effects (modal mounts, focus moves) happen via
    /// `&mut self` and aren't reflected in the return.
    pub fn dispatch_action(
        &mut self,
        action: &lazybox_tui_core::action::Action,
    ) -> Vec<IpcCommand> {
        use lazybox_tui_core::action::{Action, ActionDef};
        if Self::hopper_action_requires_project(action) {
            let repo_less_hopper = self.sidebar.selected_workspace().and_then(|workspace| {
                (workspace.hopper.is_some() && workspace.project_key.is_none())
                    .then(|| workspace.key.clone())
            });
            if let Some(workspace) = repo_less_hopper {
                self.mount_hopper_project_picker(workspace, action.clone());
                return Vec::new();
            }
        }
        // `g m` on a single PR lazybox already knows is conflicting is a
        // doomed dispatch — GitHub would only reject it. Skip straight to
        // the one-key resolve prompt rather than a merge confirm that can
        // only fail (#947). Only the single-target case: a `v` bulk merge
        // (#899) falls through to the fan-out, which already reports each
        // conflicting PR as skipped in its confirm split.
        if matches!(action, Action::MergePr) && !self.bulk_active() {
            let conflict_target = self.sidebar.selected_workspace().and_then(|ws| {
                ws.pr
                    .as_ref()
                    .filter(|pr| pr.mergeable.is_conflicting())
                    .map(|pr| (ws.key.clone(), pr.id.key.clone()))
            });
            if let Some((workspace, pr_label)) = conflict_target {
                self.mount_conflict_resolve(&workspace, &pr_label);
                return Vec::new();
            }
        }
        // Destructive gate, type-system enforced via the catalog.
        // Every destructive action is routed through the unified
        // Confirm modal first; the pending action lives in
        // `ModalFlow::ActionConfirm` and fires on `Msg::Confirmed(true)`.
        // This is the *only* path through `dispatch_action` for
        // destructive variants — there's no way to fire one
        // without the user confirming.
        if ActionDef::for_action(action).is_destructive() {
            // Resolve the concrete target set NOW, while the selection
            // is what the user acted on. A `v` multi-selection targets
            // every marked row for the bulk-appropriate destructive
            // actions (archive / merge / long-snooze / delete-or-close /
            // close-issue — the latter two joined the set in #1243:
            // `g d` with rows marked used to silently act on ONE); only
            // the inherently single-target on-main spawns stay on the
            // focused row even under a selection. The confirm fires
            // against this stash — see `ModalFlow::ActionConfirm` —
            // never the live selection, so a cursor drift under the
            // modal can't redirect it.
            let targets = self.resolve_confirm_targets(action);
            if targets.is_empty() {
                // Nothing focused to act on. The catalog's
                // availability gate keeps surfaces from offering the
                // action here; drop silently like the unchecked path
                // would have.
                return Vec::new();
            }
            // Archive is the only destructive action that applies to a
            // project header (it deletes the project + cascades). Any
            // other — e.g. `x z` long-snooze pressed while the
            // cursor sits on a project header with no workspace
            // selected — has nothing to act on, so drop it silently
            // rather than mount a confirm that would no-op on Yes. A
            // project header never lands in a multi-select set, so this
            // only fires on the single focused-header case.
            if targets
                .iter()
                .all(|t| matches!(t, ActionConfirmTarget::Project(_)))
                && !matches!(action, lazybox_tui_core::action::Action::Archive)
            {
                return Vec::new();
            }
            // Single target keeps its context-sensitive copy (project
            // archive, delete/close naming its exact issue/PR); a bulk
            // set renders the count + affected list + eligible/skipped
            // split. Falls back to the static catalog prompt otherwise.
            let custom_prompt = if targets.len() > 1 {
                Some(self.bulk_confirm_prompt(action, &targets))
            } else {
                self.action_confirm_override(action, targets.first())
            };
            self.mount_action_confirm(action.clone(), targets, custom_prompt);
            return Vec::new();
        }
        self.dispatch_action_unchecked(action)
    }

    fn hopper_action_requires_project(action: &lazybox_tui_core::action::Action) -> bool {
        use lazybox_tui_core::action::Action;
        matches!(
            action,
            Action::Work
                | Action::WorkWith(_)
                | Action::SpawnAgent(_)
                | Action::WorkTier(_)
                | Action::SpawnTier(_)
                | Action::SpawnAgentRemote(_)
                | Action::SpawnShell
                | Action::SpawnAgentOnMain(_)
                | Action::SpawnShellOnMain
                | Action::OpenEditor
                | Action::OpenWith
                | Action::OpenWithApp(_)
                | Action::ViewDiff
        )
    }

    /// Resolve what a destructive action mounted right now would act
    /// on: the selected workspace row, or (for project-header focus)
    /// the focused project. None when neither is focused.
    fn resolve_action_confirm_target(&self) -> Option<ActionConfirmTarget> {
        if let Some(sk) = self.sidebar.selected_workspace_key() {
            return Some(ActionConfirmTarget::Workspace(sk.clone()));
        }
        self.sidebar
            .focused_project_key()
            .map(ActionConfirmTarget::Project)
    }

    /// The one shared target-resolution helper every bulk-capable
    /// workspace action reads (#899): a non-empty `v` multi-selection
    /// resolves to all marked rows (sidebar order), else the focused
    /// row. Empty when neither is present. A new workspace action opts
    /// into multi-select for free by resolving its targets here.
    pub(super) fn resolve_targets(&self) -> Vec<lazybox_core::SessionKey> {
        let selected = self.sidebar.selected_broadcast_keys();
        if !selected.is_empty() {
            return selected;
        }
        self.sidebar
            .selected_workspace_key()
            .cloned()
            .into_iter()
            .collect()
    }

    /// Whether any workspace in the current r-spawn target set actually has
    /// a box (its repo didn't opt out, #1066). Drives the bulk hard-gate: an
    /// all-disabled selection must not force a needless connect just to skip
    /// every target.
    fn selection_has_remote_target(&self) -> bool {
        self.resolve_targets().iter().any(|key| {
            self.sidebar
                .workspace_by_key(key)
                .is_some_and(|ws| self.remote_for_repo(ws.repo_slug().as_deref()).is_some())
        })
    }

    /// The target *set* a destructive action fires against: every
    /// `v`-marked row when a multi-selection is active **and the action
    /// is bulk-appropriate** ([`is_bulk_destructive`]), else the single
    /// focused row / project header. Sidebar (visible) order (#899).
    /// Gating on `is_bulk_destructive` keeps the inherently single-target
    /// destructive actions (the on-main spawns) focused-only even under
    /// a selection — a bulk set would otherwise reach them with the
    /// generic archive prompt and fire them across the whole selection.
    fn resolve_confirm_targets(
        &self,
        action: &lazybox_tui_core::action::Action,
    ) -> Vec<ActionConfirmTarget> {
        if is_bulk_destructive(action) {
            let selected = self.sidebar.selected_broadcast_keys();
            if !selected.is_empty() {
                return selected
                    .into_iter()
                    .map(ActionConfirmTarget::Workspace)
                    .collect();
            }
        }
        self.resolve_action_confirm_target().into_iter().collect()
    }

    /// Whether a `v` multi-selection is active — the signal every
    /// bulk-capable action reads to decide selection-or-focused. Marks
    /// on rows hidden by the current mailbox / filter don't count, so
    /// this stays in lockstep with the on-screen gutter (#786).
    pub(super) fn bulk_active(&self) -> bool {
        !self.sidebar.selected_broadcast_keys().is_empty()
    }

    /// Display name for a workspace key, for bulk summaries and the
    /// affected-list in confirm copy. Falls back to the raw key when the
    /// row has gone.
    fn workspace_display_name(&self, key: &lazybox_core::SessionKey) -> String {
        self.sidebar
            .workspace_by_key(key)
            .map(|w| crate::util::notice_slug(&w.name).into_owned())
            .unwrap_or_else(|| key.to_string())
    }

    /// Compose the confirm-modal prompt for a bulk destructive action:
    /// count + a truncated list of what will be affected, and (where an
    /// eligibility gate applies, e.g. merge) the "N will run, M skipped"
    /// split so a bulk destructive action isn't a blind "Y" (#899).
    pub(super) fn bulk_confirm_prompt(
        &self,
        action: &lazybox_tui_core::action::Action,
        targets: &[ActionConfirmTarget],
    ) -> String {
        use lazybox_tui_core::action::Action;
        let names: Vec<String> = targets
            .iter()
            .map(|t| match t {
                ActionConfirmTarget::Workspace(k) => self.workspace_display_name(k),
                ActionConfirmTarget::Project(k) => k.as_str().to_string(),
            })
            .collect();
        let n = names.len();
        let list = truncate_affected_list(&names);
        match action {
            Action::MergePr => {
                let ready = targets
                    .iter()
                    .filter(|t| match t {
                        ActionConfirmTarget::Workspace(k) => {
                            self.sidebar.workspace_by_key(k).is_some_and(|w| {
                                matches!(
                                    crate::intent::resolve_merge(Some(w)),
                                    crate::intent::Intent::MergePr { .. }
                                )
                            })
                        }
                        ActionConfirmTarget::Project(_) => false,
                    })
                    .count();
                let skipped = n - ready;
                if ready == 0 {
                    format!(
                        "None of the {n} selected PRs are merge-ready — merge anyway is a no-op. {list}"
                    )
                } else if skipped == 0 {
                    format!("Merge {ready} PRs? {list}")
                } else {
                    format!(
                        "Merge {ready} of {n} selected PRs? {skipped} will be skipped (not merge-ready). {list}"
                    )
                }
            }
            Action::LongSnooze => {
                format!("Snooze {n} workspaces for a long time? {list}")
            }
            Action::Archive => {
                format!("Archive {n} workspaces? Any running sessions are killed. {list}")
            }
            // Mixed set: an open PR is closed, an open issue deleted —
            // name both verbs with their counts so the split is explicit
            // ("Close 3 PRs and delete 1 issue? 1 skipped …"). Same
            // eligibility predicate the single-target path re-checks at
            // Yes-time, so prompt and outcome can't disagree.
            Action::DeleteOrClose => {
                let plural = |n: usize| if n == 1 { "" } else { "s" };
                let mut close_prs = 0usize;
                let mut delete_issues = 0usize;
                for t in targets {
                    if let ActionConfirmTarget::Workspace(k) = t
                        && let Some(ws) = self.sidebar.workspace_by_key(k)
                        && lazybox_tui_core::action::availability(
                            lazybox_tui_core::action::ActionKind::DeleteOrClose,
                            Some(ws),
                        )
                    {
                        if ws.pr.is_some() {
                            close_prs += 1;
                        } else {
                            delete_issues += 1;
                        }
                    }
                }
                let skipped = n - close_prs - delete_issues;
                if close_prs + delete_issues == 0 {
                    format!(
                        "None of the {n} selected are open to close or delete — nothing will happen. {list}"
                    )
                } else {
                    let mut verbs: Vec<String> = Vec::new();
                    if close_prs > 0 {
                        verbs.push(format!(
                            "close {close_prs} PR{} without merging",
                            plural(close_prs)
                        ));
                    }
                    if delete_issues > 0 {
                        verbs.push(format!(
                            "delete {delete_issues} issue{}",
                            plural(delete_issues)
                        ));
                    }
                    let mut prompt = capitalize_first(&verbs.join(" and "));
                    prompt.push('?');
                    if skipped > 0 {
                        prompt.push_str(&format!(" {skipped} will be skipped (no longer open)."));
                    }
                    format!("{prompt} {list}")
                }
            }
            Action::CloseIssue => {
                let ready = targets
                    .iter()
                    .filter(|t| match t {
                        ActionConfirmTarget::Workspace(k) => {
                            self.sidebar.workspace_by_key(k).is_some_and(|ws| {
                                lazybox_tui_core::action::availability(
                                    lazybox_tui_core::action::ActionKind::CloseIssue,
                                    Some(ws),
                                )
                            })
                        }
                        ActionConfirmTarget::Project(_) => false,
                    })
                    .count();
                let skipped = n - ready;
                if ready == 0 {
                    format!(
                        "None of the {n} selected have an open issue to close — nothing will happen. {list}"
                    )
                } else if skipped == 0 {
                    format!("Close {ready} issues as not-planned? {list}")
                } else {
                    format!(
                        "Close {ready} of {n} selected issues as not-planned? {skipped} will be skipped (no open issue). {list}"
                    )
                }
            }
            // Every selected row was gated on the same open-issue/PR
            // predicate as delete-or-close, so all N get both the upstream
            // close and the local archive.
            Action::CloseAndArchive => {
                format!(
                    "Close/delete and archive {n} workspaces? Each issue is deleted \
                     (or closed as not-planned) or its PR closed without merging, its \
                     sessions killed, and the row dropped. {list}"
                )
            }
            // Only the `is_bulk_destructive` actions reach a bulk confirm;
            // a neutral fallback keeps a future mis-wiring from lying
            // ("Archive N…") about what a different action does.
            _ => format!("Apply this action to {n} workspaces? {list}"),
        }
    }

    /// Carry out a destructive action the user just confirmed,
    /// against the target stashed at mount time. The stash — not the
    /// live sidebar selection — names the row the prompt described;
    /// if it no longer exists (removed by a daemon event while the
    /// modal was up) this no-ops with a footer notice instead of
    /// firing at whatever the cursor drifted onto.
    pub(crate) fn dispatch_action_confirmed(
        &mut self,
        action: &lazybox_tui_core::action::Action,
        target: &ActionConfirmTarget,
    ) -> Vec<IpcCommand> {
        use lazybox_tui_core::action::Action;
        match target {
            ActionConfirmTarget::Workspace(session_key) => {
                let workspace = self.sidebar.workspace_by_key(session_key).cloned();
                if workspace.is_none() {
                    self.flash_info("workspace is gone — nothing to do");
                    return Vec::new();
                }
                match action {
                    Action::Archive => {
                        // Optimistic: drop the row now so archive feels
                        // instant instead of waiting for the daemon's
                        // `WorkspaceRemoved` echo. A failed delete
                        // re-inserts it (#476).
                        self.optimistic_remove_workspace(session_key);
                        vec![IpcCommand::Kill {
                            session_key: session_key.clone(),
                        }]
                    }
                    Action::CloseIssue => match workspace.as_ref() {
                        // Re-check against the STASHED workspace — a poll
                        // could have closed the issue or attached a PR
                        // while the modal was up. Same predicate the
                        // catalog gates the keypress on, so the two never
                        // disagree.
                        Some(ws)
                            if lazybox_tui_core::action::availability(
                                lazybox_tui_core::action::ActionKind::CloseIssue,
                                Some(ws),
                            ) =>
                        {
                            // Pending feedback at command send — the
                            // provider round trip takes seconds, and a
                            // silent gap after "Yes" reads as a dropped
                            // keypress (same convention as CollapseIntoPr's
                            // "joining into PR…").
                            self.flash_info(format!(
                                "closing issue{}…",
                                task_number_suffix(
                                    ws.gh_issues
                                        .iter()
                                        .chain(ws.linear_issues.iter())
                                        .next()
                                        .map(|i| i.id.key.as_str())
                                        .unwrap_or("")
                                )
                            ));
                            vec![IpcCommand::CloseIssue {
                                workspace_key: ws.key.clone(),
                            }]
                        }
                        _ => {
                            self.flash_info("issue is no longer open — nothing to close");
                            Vec::new()
                        }
                    },
                    Action::DeleteOrClose => match workspace.as_ref() {
                        // Re-check against the STASHED workspace — the
                        // item may have merged/closed or the workspace
                        // changed shape while the modal was up. Same
                        // predicate the catalog gates the keypress on.
                        Some(ws)
                            if lazybox_tui_core::action::availability(
                                lazybox_tui_core::action::ActionKind::DeleteOrClose,
                                Some(ws),
                            ) =>
                        {
                            // Pending feedback at command send — the
                            // provider round trip takes seconds (same
                            // convention as merge/close).
                            if let Some(pr) = ws.pr.as_ref() {
                                self.flash_info(format!(
                                    "closing PR{}…",
                                    task_number_suffix(&pr.id.key)
                                ));
                            } else {
                                self.flash_info(format!(
                                    "deleting issue{}…",
                                    task_number_suffix(
                                        ws.gh_issues
                                            .first()
                                            .map(|i| i.id.key.as_str())
                                            .unwrap_or("")
                                    )
                                ));
                            }
                            vec![IpcCommand::DeleteOrClose {
                                workspace_key: ws.key.clone(),
                            }]
                        }
                        _ => {
                            self.flash_info(
                                "the issue / PR is no longer open — nothing to delete or close",
                            );
                            Vec::new()
                        }
                    },
                    // The combined `g d` + `x x`: close/delete the item
                    // upstream AND archive the workspace in one go. Re-check
                    // the same predicate the keypress was gated on, then emit
                    // both commands. Archive optimistically (like Action::Archive)
                    // so the row drops instantly; the daemon's DeleteOrClose is
                    // best-effort upstream — the local kill is what ends the
                    // workspace either way.
                    Action::CloseAndArchive => match workspace.as_ref() {
                        Some(ws)
                            if lazybox_tui_core::action::availability(
                                lazybox_tui_core::action::ActionKind::CloseAndArchive,
                                Some(ws),
                            ) =>
                        {
                            if let Some(pr) = ws.pr.as_ref() {
                                self.flash_info(format!(
                                    "closing PR{} & killing workspace…",
                                    task_number_suffix(&pr.id.key)
                                ));
                            } else {
                                self.flash_info(format!(
                                    "deleting issue{} & killing workspace…",
                                    task_number_suffix(
                                        ws.gh_issues
                                            .first()
                                            .map(|i| i.id.key.as_str())
                                            .unwrap_or("")
                                    )
                                ));
                            }
                            let workspace_key = ws.key.clone();
                            self.optimistic_remove_workspace(session_key);
                            vec![
                                IpcCommand::DeleteOrClose { workspace_key },
                                IpcCommand::Kill {
                                    session_key: session_key.clone(),
                                },
                            ]
                        }
                        _ => {
                            self.flash_info(
                                "the issue / PR is no longer open — use archive (x x) to just kill the workspace",
                            );
                            Vec::new()
                        }
                    },
                    Action::LongSnooze => {
                        let intent = crate::intent::resolve_long_snooze(
                            workspace.as_ref(),
                            self.ui_defaults.long_snooze,
                        );
                        self.execute_dispatch_intent(intent, workspace.as_ref())
                    }
                    Action::ResetAgentContext => match workspace.as_ref() {
                        Some(ws) => {
                            let target = self
                                .sidebar
                                .agent_terminal_for(&lazybox_core::SessionKey::from(&ws.key));
                            match target {
                                Some((terminal_id, agent_id)) => {
                                    let cmd = lazybox_tui_core::agents::registry()
                                        .get(&agent_id)
                                        .and_then(|a| a.clear_context_command());
                                    match cmd {
                                        Some(cmd) => {
                                            self.flash_info(format!(
                                                "resetting {agent_id} context ({cmd})…"
                                            ));
                                            vec![IpcCommand::InjectPrompt {
                                                terminal_id,
                                                prompt: cmd.to_string(),
                                                fallback_spawn: None,
                                                submit: true,
                                            }]
                                        }
                                        None => {
                                            self.flash_info(format!(
                                                "{agent_id} has no context-reset command"
                                            ));
                                            Vec::new()
                                        }
                                    }
                                }
                                None => {
                                    self.flash_info(
                                        "no running agent on this workspace — a c starts one",
                                    );
                                    Vec::new()
                                }
                            }
                        }
                        _ => Vec::new(),
                    },
                    Action::MergePr => {
                        // Structural re-check against the STASHED
                        // workspace (the PR may have merged/closed
                        // while the modal was up). Cached soft state
                        // never refuses (#1203) — GitHub is the
                        // authority at merge time; a cached block
                        // becomes an ADVISORY on the send, and a real
                        // rejection comes back as `PrMergeFailed` with
                        // GitHub's reason.
                        let intent = crate::intent::resolve_merge(workspace.as_ref());
                        if matches!(intent, crate::intent::Intent::MergePr { .. }) {
                            if let Some(reason) =
                                crate::intent::merge_send_advisory(workspace.as_ref())
                            {
                                self.flash_info(format!(
                                    "cached state says {reason} — asking GitHub anyway"
                                ));
                            }
                            self.execute_dispatch_intent(intent, workspace.as_ref())
                        } else {
                            self.flash_info("can't merge: the PR isn't open");
                            Vec::new()
                        }
                    }
                    // On-main spawns fire against the STASHED workspace,
                    // not the live selection — a daemon event could have
                    // drifted the sidebar cursor while the confirm was up,
                    // and `dispatch_action_unchecked` would otherwise
                    // launch a shared-branch session on whatever the
                    // cursor landed on. `on_main` always targets the
                    // shared checkout, so `session_id` is None.
                    Action::SpawnAgentOnMain(agent_id) => vec![IpcCommand::Spawn {
                        model_alias: None,
                        access: lazybox_ipc::AgentRunAccess::Default,
                        session_key: session_key.clone(),
                        session_id: None,
                        client_request_id: None,
                        kind: lazybox_ipc::TerminalKind::Agent(agent_id.clone()),
                        cwd: None,
                        initial_prompt: None,
                        initial_snippet: None,
                        on_main: true,
                        // `b c` / `b x` / `b u` explicit agent spawn (#1310).
                        force_new: true,
                    }],
                    Action::SpawnShellOnMain => vec![IpcCommand::Spawn {
                        model_alias: None,
                        access: lazybox_ipc::AgentRunAccess::Default,
                        session_key: session_key.clone(),
                        session_id: None,
                        client_request_id: None,
                        kind: lazybox_ipc::TerminalKind::Shell,
                        cwd: None,
                        initial_prompt: None,
                        initial_snippet: None,
                        on_main: true,
                        force_new: false,
                    }],
                    // A future destructive action that hasn't grown a
                    // targeted arm yet falls back to the legacy
                    // selection-based dispatch.
                    other => self.dispatch_action_unchecked(other),
                }
            }
            ActionConfirmTarget::Project(project_key) => {
                if !self.projects.contains_key(project_key) {
                    self.flash_info("project is gone — nothing to do");
                    return Vec::new();
                }
                match action {
                    Action::Archive => {
                        // Optimistic: drop the project header + its child
                        // rows now; a failed cascade re-inserts them all
                        // (#476).
                        self.optimistic_remove_project(project_key);
                        vec![IpcCommand::DeleteProject {
                            project_key: project_key.clone(),
                        }]
                    }
                    other => self.dispatch_action_unchecked(other),
                }
            }
        }
    }

    /// Carry out a confirmed destructive action over a bulk target set
    /// (#899). Each target runs through the per-target
    /// `dispatch_action_confirmed`, so its eligibility re-check and
    /// optimistic UI are unchanged; a target that yields no command
    /// (gone, or no longer eligible — merged, conflicted) is counted as
    /// skipped. The multi-select is cleared and a single aggregate
    /// summary replaces the per-target chatter. Iterates the *snapshot*
    /// captured at mount, never the live selection.
    pub(crate) fn dispatch_action_confirmed_bulk(
        &mut self,
        action: &lazybox_tui_core::action::Action,
        targets: &[ActionConfirmTarget],
    ) -> Vec<IpcCommand> {
        let mut cmds = Vec::new();
        let mut acted = 0usize;
        let mut skipped: Vec<String> = Vec::new();
        for target in targets {
            let produced = self.dispatch_action_confirmed(action, target);
            if produced.is_empty() {
                skipped.push(match target {
                    ActionConfirmTarget::Workspace(k) => self.workspace_display_name(k),
                    ActionConfirmTarget::Project(k) => k.as_str().to_string(),
                });
            } else {
                acted += 1;
                cmds.extend(produced);
            }
        }
        // Preserve the marks when nothing acted (every target had
        // regressed / gone ineligible under the modal) so the user can
        // retry — the same no-op-survives rule `bulk_dispatch` follows.
        if acted > 0 {
            self.sidebar.clear_broadcast_selection();
        }
        self.flash_bulk_summary(bulk_confirmed_verb(action), "ineligible", acted, &skipped);
        self.redraw = true;
        cmds
    }

    /// Fan a per-workspace IPC command over the active `v` multi-select
    /// (#899). `build` yields the command for an eligible workspace or
    /// `None` to skip it; the shared loop collects commands, clears the
    /// selection when anything acted, and flashes a
    /// "<done> N · M skipped (<why>)" summary. Only called when
    /// [`bulk_active`](Self::bulk_active) — the single-row path stays in
    /// each action's own arm so its bespoke UX (pickers, optimistic
    /// redraws) is unchanged.
    fn bulk_dispatch<F>(&mut self, done: &str, why_skip: &str, build: F) -> Vec<IpcCommand>
    where
        F: Fn(&lazybox_core::Workspace) -> Option<IpcCommand>,
    {
        let keys = self.resolve_targets();
        let mut cmds = Vec::new();
        let mut skipped: Vec<String> = Vec::new();
        for key in &keys {
            // A row that vanished between mark and fire is dropped
            // silently — there's nothing to name in the summary.
            if let Some(ws) = self.sidebar.workspace_by_key(key) {
                match build(ws) {
                    Some(cmd) => cmds.push(cmd),
                    None => skipped.push(crate::util::notice_slug(&ws.name).into_owned()),
                }
            }
        }
        let acted = cmds.len();
        if acted > 0 {
            self.sidebar.clear_broadcast_selection();
        }
        self.flash_bulk_summary(done, why_skip, acted, &skipped);
        self.redraw = true;
        cmds
    }

    /// Flash the shared bulk outcome notice: "<done> N workspaces · M
    /// skipped (<why>): a, b". Empty when nothing at all happened.
    fn flash_bulk_summary(&mut self, done: &str, why_skip: &str, acted: usize, skipped: &[String]) {
        let plural = |n: usize| if n == 1 { "" } else { "s" };
        let mut parts: Vec<String> = Vec::new();
        if acted > 0 {
            parts.push(format!("{done} {acted} workspace{}", plural(acted)));
        }
        if !skipped.is_empty() {
            parts.push(format!(
                "{} skipped ({why_skip}): {}",
                skipped.len(),
                truncate_affected_list(skipped),
            ));
        }
        let summary = if parts.is_empty() {
            format!("nothing to {done}")
        } else {
            parts.join(" · ")
        };
        self.flash_info(summary);
    }

    /// Internal: actually carry out an action without checking the
    /// destructive flag. Public `dispatch_action` gates on
    /// `is_destructive` and routes through the Confirm modal for
    /// the destructive ones — this method is what the modal's
    /// `Msg::Confirmed(true)` handler calls AFTER the user
    /// approved.
    ///
    /// Callers OTHER than `dispatch_action` and the
    /// `ActionConfirm` Yes-handler must not exist. Keeping it
    /// `pub(crate)` so the type system makes that hard to break.
    /// Pick a tailored confirm-modal prompt for the destructive
    /// `action` against its single resolved `target`. Returns `None` to
    /// fall back to the static `ActionDef::confirm_prompt`.
    ///
    /// The copy names the *stashed* target, not the cursor row: a
    /// one-row `v` selection can differ from the row the cursor sits on,
    /// and the prompt must describe what Yes will actually destroy
    /// (#1243). `None` (no stash) falls back to the cursor.
    ///
    /// Keeps catalog defaults declarative and the context-sensitive
    /// copy (project archive, delete/close naming its exact issue/PR)
    /// out of the dispatch.
    pub(super) fn action_confirm_override(
        &self,
        action: &lazybox_tui_core::action::Action,
        target: Option<&ActionConfirmTarget>,
    ) -> Option<String> {
        use lazybox_tui_core::action::Action;
        let target_ws_key = match target {
            Some(ActionConfirmTarget::Workspace(k)) => Some(k),
            _ => None,
        };
        // Stack-aware merge (issue #969): merging a PR that is stacked on
        // a still-open parent lands it out of order — GitHub then
        // retargets the parent's other children onto the grandparent/main,
        // so the rest of the stack must be restacked. Warn before the
        // merge instead of letting the user discover it after. The bottom
        // of a stack (no open parent) merges with the default prompt.
        if matches!(action, Action::MergePr) {
            let sk = target_ws_key.or_else(|| self.sidebar.selected_workspace_key())?;
            if let Some(stack) = self.sidebar.stack_info(sk)
                && let Some(parent) = stack.parent.as_ref().and_then(|p| p.number())
            {
                let this = self
                    .sidebar
                    .workspace_by_key(sk)
                    .and_then(|w| w.pr.as_ref())
                    .and_then(|pr| pr.id.number());
                let this = this.map_or_else(|| "this PR".to_string(), |n| format!("#{n}"));
                return Some(format!(
                    "{this} is stacked on #{parent}, which is still open. Merging it \
                     first lands the stack out of order — GitHub will retarget the rest \
                     onto its base, so you'll need to restack (update branch) the \
                     children. Merge {this} anyway?"
                ));
            }
            return None;
        }
        // Delete/close names its exact target — the number + title of
        // the issue/PR the confirmed keypress destroys — so the modal
        // never asks about an ambiguous "this".
        if matches!(action, Action::DeleteOrClose) {
            let sk = target_ws_key.or_else(|| self.sidebar.selected_workspace_key())?;
            let ws = self.sidebar.workspace_by_key(sk)?;
            return Some(match ws.pr.as_ref() {
                Some(pr) => format!(
                    "Close PR {} — \"{}\" — without merging? Reopen on GitHub to undo.",
                    pr.id.key,
                    truncate_title(&pr.title),
                ),
                None => {
                    let issue = ws.gh_issues.first()?;
                    format!(
                        "Delete issue {} — \"{}\"? Deletion is permanent; without admin \
                         rights it is closed as not-planned instead.",
                        issue.id.key,
                        truncate_title(&issue.title),
                    )
                }
            });
        }
        if !matches!(action, Action::Archive) {
            return None;
        }
        // Workspace target → use the default prompt.
        if target_ws_key.is_some()
            || (target.is_none() && self.sidebar.selected_workspace_key().is_some())
        {
            return None;
        }
        // Project target / header focus → custom phrasing.
        let project_key = match target {
            Some(ActionConfirmTarget::Project(k)) => k.clone(),
            _ => self.sidebar.focused_project_key()?,
        };
        let project_label = self
            .sidebar
            .project_label_for(&project_key)
            .unwrap_or_else(|| project_key.as_str().to_string());
        let child_count = self.sidebar.workspaces_in_project(&project_key);
        Some(match child_count {
            0 => format!("Delete project `{project_label}`?"),
            1 => format!(
                "Delete project `{project_label}`? Its 1 workspace + any running sessions will be killed."
            ),
            n => format!(
                "Delete project `{project_label}`? Its {n} workspaces + any running sessions will be killed."
            ),
        })
    }

    pub(crate) fn dispatch_action_unchecked(
        &mut self,
        action: &lazybox_tui_core::action::Action,
    ) -> Vec<IpcCommand> {
        use lazybox_tui_core::action::Action;
        let mut cmds = Vec::new();
        // Workspace-scoped actions need a target — grab the
        // sidebar's selection. Mismatch (no selection) silently
        // drops the action; the catalog's `availability` gates the
        // surface from offering it in that state.
        let session_key = self.sidebar.selected_workspace_key().cloned();
        // `session_id` is non-None when the cursor sits on a
        // session sub-row of a workspace; passing it makes the
        // daemon target that specific session instead of picking
        // / creating one. Matches the sidebar's existing spawn
        // handlers — without this, `a c` / `s` on a focused session
        // would silently spawn into the wrong session.
        let session_id = self.sidebar.selected_session_id();
        match action {
            Action::SpawnShell => {
                if self.bulk_active() {
                    return self.dispatch_bulk_agent(BulkOp::SpawnShell, None);
                }
                if let Some(sk) = session_key {
                    cmds.push(IpcCommand::Spawn {
                        model_alias: None,
                        access: lazybox_ipc::AgentRunAccess::Default,
                        session_key: sk,
                        session_id,
                        client_request_id: None,
                        kind: lazybox_ipc::TerminalKind::Shell,
                        cwd: None,
                        initial_prompt: None,
                        initial_snippet: None,
                        on_main: false,
                        force_new: false,
                    });
                }
            }
            Action::SpawnAgent(agent_id) => {
                if self.bulk_active() {
                    return self.dispatch_bulk_agent(BulkOp::SpawnAgent(agent_id.clone()), None);
                }
                if let Some(sk) = session_key {
                    cmds.push(IpcCommand::Spawn {
                        model_alias: None,
                        access: lazybox_ipc::AgentRunAccess::Default,
                        session_key: sk,
                        session_id,
                        client_request_id: None,
                        kind: lazybox_ipc::TerminalKind::Agent(agent_id.clone()),
                        cwd: None,
                        initial_prompt: None,
                        initial_snippet: None,
                        on_main: false,
                        // Explicit `a c` / `a x` / `a u`: always start a new
                        // agent, even beside an idle one of the same kind (#1310).
                        force_new: true,
                    });
                }
            }
            // Remote-spawn variant (`r c` / `r x` / `r u`): the same
            // agent spawn as `a c`, but routed to the remote box's client
            // (Design A) instead of `self.client`, and the workspace gets
            // a sidebar remote indicator. The box's client is an in-process
            // pipe to a worker that ensures/wakes/connects the box on this
            // first command. The command is sent directly to the remote
            // client here rather than returned in `cmds` — those flush to
            // the LOCAL daemon.
            Action::SpawnAgentRemote(agent_id) => {
                let Some(default_remote) = self.default_remote().map(str::to_string) else {
                    self.flash_error(
                        "no remote box configured — add a `sandbox:` block to spawn on a box",
                    );
                    return cmds;
                };
                // Under a live multi-select the r-spawn fans out like every
                // other bulk-appropriate spawn (#932) — same plan, each target
                // routed to (or skipped by) its repo's box per the per-project
                // opt-out. The hard-gate applies only if at least one selected
                // target actually has a box: an all-disabled selection must
                // not induce a needless (billed) connect just to skip
                // everything.
                if self.bulk_active() {
                    if self.remote_require_connect
                        && !self.remote_connected()
                        && self.selection_has_remote_target()
                    {
                        self.flash_error(
                            "box not connected — press Shift-C to connect first (require_connect is on)",
                        );
                        return cmds;
                    }
                    return self.dispatch_bulk_agent(
                        BulkOp::SpawnAgentRemote(agent_id.clone(), default_remote),
                        None,
                    );
                }
                // Per-project sandbox opt-out (#1066) is resolved FIRST — a
                // repo that set `sandbox: false` has no box, so it's refused
                // here without ever inducing a connect. Ordering matters: the
                // hard-gate below would otherwise tell a disabled repo to
                // "connect first", waking the billed box only to then refuse.
                let repo = self
                    .sidebar
                    .selected_workspace()
                    .and_then(|w| w.repo_slug());
                let Some(remote) = self.remote_for_repo(repo.as_deref()).map(str::to_string) else {
                    self.flash_error(
                        "sandbox is disabled for this project (repos.<repo>.sandbox: false)",
                    );
                    return cmds;
                };
                // Then the hard-gate (`sandbox.require_connect: true`, #1066):
                // refuse rather than silently trigger a multi-minute bring-up
                // from a spawn; the persistent indicator does the waiting.
                // Off by default — a spawn while disconnected lazily brings up.
                if self.remote_require_connect && !self.remote_connected() {
                    self.flash_error(
                        "box not connected — press Shift-C to connect first (require_connect is on)",
                    );
                    return cmds;
                }
                if let Some(sk) = session_key {
                    let spawn = IpcCommand::Spawn {
                        model_alias: None,
                        access: lazybox_ipc::AgentRunAccess::Default,
                        session_key: sk.clone(),
                        session_id,
                        client_request_id: None,
                        kind: lazybox_ipc::TerminalKind::Agent(agent_id.clone()),
                        cwd: None,
                        initial_prompt: None,
                        initial_snippet: None,
                        on_main: false,
                        // `r c` / `r x` / `r u` mirror `a c` (#1310).
                        force_new: true,
                    };
                    self.send_to_remote(&remote, spawn);
                    // Optimistic client-side tag so the sidebar row shows
                    // the remote indicator immediately — and latched
                    // (`remote_marks`) so the next local-daemon snapshot,
                    // which knows nothing about the box, can't wipe it.
                    self.mark_remote_latched(sk, remote);
                    self.redraw = true;
                }
            }
            // Main-checkout variants (`b c` / `b s`, confirm-guarded):
            // same spawn, but `on_main` tells the daemon to land in the
            // repo's shared main checkout instead of an isolated
            // worktree. `session_id` is deliberately dropped — "on main"
            // always targets the shared checkout, not the selected
            // session sub-row.
            Action::SpawnAgentOnMain(agent_id) => {
                if let Some(sk) = session_key {
                    cmds.push(IpcCommand::Spawn {
                        model_alias: None,
                        access: lazybox_ipc::AgentRunAccess::Default,
                        session_key: sk,
                        session_id: None,
                        client_request_id: None,
                        kind: lazybox_ipc::TerminalKind::Agent(agent_id.clone()),
                        cwd: None,
                        initial_prompt: None,
                        initial_snippet: None,
                        on_main: true,
                        // `b c` / `b x` / `b u` are explicit agent spawns too (#1310).
                        force_new: true,
                    });
                }
            }
            Action::SpawnShellOnMain => {
                if let Some(sk) = session_key {
                    cmds.push(IpcCommand::Spawn {
                        model_alias: None,
                        access: lazybox_ipc::AgentRunAccess::Default,
                        session_key: sk,
                        session_id: None,
                        client_request_id: None,
                        kind: lazybox_ipc::TerminalKind::Shell,
                        cwd: None,
                        initial_prompt: None,
                        initial_snippet: None,
                        on_main: true,
                        force_new: false,
                    });
                }
            }
            Action::Work => {
                // Default work (`w w`) targets whatever agent is already running on
                // this workspace (so it injects into an existing Codex /
                // Cursor session instead of always spawning the default),
                // falling back to the default agent when none is running.
                // Several running conversations → ask which one (#418).
                // The scoped `w c` / `w x` chords (Action::WorkWith) force
                // a specific agent.
                if self.bulk_active() {
                    return self.dispatch_bulk_agent(BulkOp::Work, None);
                }
                self.dispatch_work(session_id, None, &mut cmds);
            }
            Action::WorkWith(agent_id) => {
                if self.bulk_active() {
                    return self.dispatch_bulk_agent(BulkOp::WorkWith(agent_id.clone()), None);
                }
                self.dispatch_work_with(agent_id, session_id, None, &mut cmds);
            }
            Action::WorkTier(alias) => {
                if self.bulk_active() {
                    return self.dispatch_bulk_agent(BulkOp::Work, Some(alias.clone()));
                }
                // Flat `w S`: work on the same contextual target agent as
                // `w w`, but launch it at the picked model tier. The
                // alias is resolved against the target agent's menu daemon-
                // side, so it degrades to the default model for an agent
                // that doesn't define the tier.
                self.dispatch_work(session_id, Some(alias.clone()), &mut cmds);
            }
            Action::SpawnTier(alias) => {
                // `a S`: spawn the default agent at the picked tier.
                if self.bulk_active() {
                    let agent = self.sidebar.default_agent().to_string();
                    return self
                        .dispatch_bulk_agent(BulkOp::SpawnAgent(agent), Some(alias.clone()));
                }
                if let Some(sk) = session_key {
                    cmds.push(IpcCommand::Spawn {
                        session_key: sk,
                        session_id,
                        client_request_id: None,
                        kind: lazybox_ipc::TerminalKind::Agent(
                            self.sidebar.default_agent().to_string(),
                        ),
                        cwd: None,
                        initial_prompt: None,
                        initial_snippet: None,
                        on_main: false,
                        model_alias: Some(alias.clone()),
                        access: lazybox_ipc::AgentRunAccess::Default,
                        // `a S` / `a M` / `a L` are explicit agent spawns (#1310).
                        force_new: true,
                    });
                }
            }
            Action::OpenEditor => {
                // `open_editor` is the orchestrator's existing
                // helper — it picks the right template (single
                // editor → launch directly; multiple → mount
                // picker; none → footer notice).
                self.open_editor();
            }
            Action::OpenWith => {
                // Config-driven "Open with…" (issue #1100): surface the
                // `open_with:` apps on the focused workspace. Single app
                // → launch directly; multiple → picker; none → notice.
                self.open_with_picker();
            }
            Action::OpenWithApp(name) => {
                // Favorite key (#1100): launch that specific `open_with:`
                // app directly, skipping the picker. Same launch path as a
                // pick — remote / no-worktree gating and provisioning
                // handled in `launch_open_with`.
                self.open_with_app_by_name(name);
            }
            Action::ViewDiff => {
                if let Some((workspace_key, target)) =
                    self.sidebar.selected_workspace().and_then(|workspace| {
                        let target = session_id
                            .or_else(|| workspace.default_session().map(|session| session.id))
                            .map(lazybox_ipc::WorkspaceDiffTarget::Session)
                            .or_else(|| {
                                workspace
                                    .linked_checkout
                                    .as_ref()
                                    .map(|_| lazybox_ipc::WorkspaceDiffTarget::LinkedCheckout)
                            })?;
                        Some((workspace.key.clone(), target))
                    })
                {
                    self.pending_diff_session = Some((workspace_key.clone(), target.clone()));
                    self.flash_hint("reading worktree diff…");
                    cmds.push(IpcCommand::InspectWorkspaceDiff {
                        workspace_key,
                        target,
                    });
                } else {
                    self.flash_hint("this workspace has no worktree to review");
                }
            }
            Action::NewWorkspace => {
                let focused = self.sidebar.focused_project_key();
                // Explicit variant list (no `_` catch-all) so a new
                // Intent variant is a compile error here — this
                // consumer must decide what it means instead of
                // silently swallowing it.
                use crate::intent::Intent;
                match crate::intent::resolve_new_workspace(focused) {
                    Intent::MountNewWorkspaceInput { project_key } => {
                        self.mount_new_workspace_input(project_key);
                    }
                    Intent::Notice(msg) => {
                        self.flash_info(msg);
                    }
                    // The resolver only produces the two arms above;
                    // the rest are unreachable from this call.
                    Intent::NoOp
                    | Intent::SpawnAgent { .. }
                    | Intent::SpawnShell { .. }
                    | Intent::MountReply { .. }
                    | Intent::MountAdoptPicker { .. }
                    | Intent::OpenEditor
                    | Intent::MergePr { .. }
                    | Intent::UpdateBranch { .. }
                    | Intent::SetAutoMergeOnGreen { .. }
                    | Intent::SetTrackMain { .. }
                    | Intent::KillWorkspace { .. }
                    | Intent::Snooze { .. }
                    | Intent::Unsnooze { .. }
                    | Intent::MarkAllRead { .. }
                    | Intent::MarkActivitiesRead { .. }
                    | Intent::CollapseIntoPr { .. }
                    | Intent::MountHandoffPicker { .. } => {}
                }
            }
            Action::RenameWorkspace => {
                // A cursor parked on a Space header renames the Space
                // (#1211); anywhere else, rename targets the focused
                // workspace's display label (any workspace, even
                // session-less). Section::Workspace, so this fires from
                // both Sidebar and Right focus.
                if let Some((false, space)) = self.sidebar.cursor_header() {
                    self.mount_rename_space_input(space);
                } else if let Some(ws) = self.sidebar.selected_workspace() {
                    let session_key: lazybox_core::SessionKey = (&ws.key).into();
                    self.mount_rename_workspace_input(session_key);
                }
            }
            Action::MoveToSpace => {
                // Assign the repo group at/above the cursor to a Space.
                // The group resolves from a header row too, so this is
                // reachable whether the cursor is on the header or a
                // workspace under it.
                if let Some(source) = self.sidebar.cursor_repo() {
                    // Picker over hand-created Spaces (#1206); falls
                    // back to the free-text input when none exist yet.
                    self.mount_move_to_space_picker(source);
                } else {
                    self.flash_info("select a repo group first".to_string());
                }
            }
            Action::NewProject => {
                self.mount_new_workspace_repo_picker();
            }
            Action::ImportCheckout => {
                self.start_scan_checkouts();
            }
            Action::AddScanRoot => {
                self.mount_add_scan_root_input();
            }
            Action::MarkAllRead => {
                // A sidebar multi-select marks every selected workspace
                // read at once (#899); the per-activity selection is a
                // right-pane concern and only applies to the single
                // focused row, so bulk wins when it's active.
                if self.bulk_active() {
                    return self.bulk_dispatch("marked read", "n/a", |ws| {
                        Some(IpcCommand::MarkRead {
                            session_key: lazybox_core::SessionKey::from(&ws.key),
                        })
                    });
                }
                let workspace = self.sidebar.selected_workspace().cloned();
                let selected = self.right.selected_activity_indices();
                let cursor = (self.focus == PaneFocus::Right)
                    .then(|| self.right.activity_cursor_target())
                    .flatten();
                let intent =
                    crate::intent::resolve_mark_read_targets(workspace.as_ref(), &selected, cursor);
                cmds.extend(self.execute_dispatch_intent(intent, workspace.as_ref()));
            }
            Action::Archive => {
                // Destructive — normally routed through
                // `dispatch_action_confirmed` with a mount-time
                // target; this selection-based arm is the fallback.
                //
                // Polymorphic by focused row: cursor on a workspace /
                // session row deletes that workspace; cursor on a
                // project header (RepoHeader) deletes the whole
                // project and cascades to its workspaces. The
                // availability gate (`availability` in the catalog)
                // already ensures one of the two has a target.
                if let Some(sk) = session_key {
                    self.optimistic_remove_workspace(&sk);
                    cmds.push(IpcCommand::Kill { session_key: sk });
                } else if let Some(project_key) = self.sidebar.focused_project_key() {
                    self.optimistic_remove_project(&project_key);
                    cmds.push(IpcCommand::DeleteProject { project_key });
                }
            }
            Action::AdoptSessions => {
                // Resolver decides "has sessions to adopt?": yes
                // → mount the target picker; no → footer notice.
                // Same shape as the inline handler had.
                let workspace = self.sidebar.selected_workspace().cloned();
                // Explicit variant list — a new Intent variant must be
                // triaged here at compile time, not silently dropped.
                use crate::intent::Intent;
                match crate::intent::resolve_adopt(workspace.as_ref()) {
                    Intent::MountAdoptPicker { source_key } => {
                        self.mount_adopt_picker(source_key);
                    }
                    Intent::Notice(msg) => {
                        self.flash_info(msg);
                    }
                    Intent::NoOp
                    | Intent::SpawnAgent { .. }
                    | Intent::SpawnShell { .. }
                    | Intent::MountReply { .. }
                    | Intent::MountNewWorkspaceInput { .. }
                    | Intent::OpenEditor
                    | Intent::MergePr { .. }
                    | Intent::UpdateBranch { .. }
                    | Intent::SetAutoMergeOnGreen { .. }
                    | Intent::SetTrackMain { .. }
                    | Intent::KillWorkspace { .. }
                    | Intent::Snooze { .. }
                    | Intent::Unsnooze { .. }
                    | Intent::MarkAllRead { .. }
                    | Intent::MarkActivitiesRead { .. }
                    | Intent::CollapseIntoPr { .. }
                    | Intent::MountHandoffPicker { .. } => {}
                }
            }
            Action::CollapseIntoPr => {
                let issue_workspace = self.sidebar.selected_workspace().cloned();
                let workspaces = self.sidebar.workspaces_iter().collect::<Vec<_>>();
                let intent =
                    crate::intent::resolve_collapse_into_pr(issue_workspace.as_ref(), &workspaces);
                cmds.extend(self.execute_dispatch_intent(intent, issue_workspace.as_ref()));
            }
            Action::ToggleSnooze => {
                // When the workspace is already snoozed, `z` toggles
                // it off (no picker — that'd be friction). When NOT
                // snoozed, mount the duration picker so the user can
                // pick something meaningful instead of paying the
                // YAML default every time.
                let now = chrono::Utc::now();
                // Bulk (#899): toggle each selected row against its own
                // current state — snooze the awake, wake the snoozed —
                // so a mixed selection resolves per-row without a picker.
                if self.bulk_active() {
                    let until = now
                        + chrono::Duration::from_std(self.ui_defaults.short_snooze)
                            .unwrap_or_else(|_| chrono::Duration::hours(4));
                    return self.bulk_dispatch("updated snooze on", "n/a", move |ws| {
                        let session_key = lazybox_core::SessionKey::from(&ws.key);
                        Some(if ws.is_snoozed(now) {
                            IpcCommand::Unsnooze { session_key }
                        } else {
                            IpcCommand::Snooze { session_key, until }
                        })
                    });
                }
                let Some(workspace) = self.sidebar.selected_workspace().cloned() else {
                    return cmds;
                };
                if workspace.is_snoozed(now) {
                    let session_key = lazybox_core::SessionKey::from(&workspace.key);
                    cmds.push(IpcCommand::Unsnooze { session_key });
                } else {
                    let session_key = lazybox_core::SessionKey::from(&workspace.key);
                    self.mount_snooze_picker(session_key);
                }
            }
            Action::MergePr => {
                // Destructive — normally routed through
                // `dispatch_action_confirmed` with a mount-time
                // target; this selection-based arm is the fallback.
                // Re-check the merge preconditions defensively, then
                // fire the IPC. (Catalog availability gates the surface
                // from offering the action when CI / review /
                // conflict state isn't ready, so this re-check
                // mostly catches the rare race where state
                // changed while the modal was open.)
                let workspace = self.sidebar.selected_workspace().cloned();
                let intent = crate::intent::resolve_merge(workspace.as_ref());
                cmds.extend(self.execute_dispatch_intent(intent, workspace.as_ref()));
            }
            Action::UpdateBranch => {
                // Non-destructive (Guard::None), so it fires straight
                // through here. A `v` multi-select updates every behind
                // PR in the set (#899); this bulk fan-out fully replaces
                // the retired `Shift-U` key (#932). Otherwise re-resolve
                // against the live focused selection.
                if self.bulk_active() {
                    return self.bulk_dispatch("updating branch of", "not behind base", |ws| {
                        ws.pr.as_ref().filter(|pr| pr.is_behind_base).map(|_| {
                            IpcCommand::UpdateBranch {
                                workspace_key: ws.key.clone(),
                            }
                        })
                    });
                }
                let workspace = self.sidebar.selected_workspace().cloned();
                let intent = crate::intent::resolve_update_branch(workspace.as_ref());
                cmds.extend(self.execute_dispatch_intent(intent, workspace.as_ref()));
            }
            Action::ToggleAutoMerge => {
                // Bulk (#899): arm auto-merge-on-green across every
                // selected PR (disarm stays a single-row toggle — the
                // useful bulk gesture is "arm all these"). Non-PR rows
                // are skipped and counted.
                if self.bulk_active() {
                    return self.bulk_dispatch("armed auto-merge on", "no PR", |ws| {
                        ws.pr.as_ref().map(|_| IpcCommand::SetAutoMergeOnGreen {
                            session_key: lazybox_core::SessionKey::from(&ws.key),
                            enabled: true,
                        })
                    });
                }
                let workspace = self.sidebar.selected_workspace().cloned();
                // Explicit variant list — a new Intent variant must be
                // triaged here at compile time, not silently dropped.
                use crate::intent::Intent;
                match crate::intent::resolve_toggle_auto_merge(workspace.as_ref()) {
                    Intent::SetAutoMergeOnGreen {
                        workspace_key,
                        enabled,
                    } => {
                        let name = workspace
                            .as_ref()
                            .map(|w| crate::util::notice_slug(&w.name).into_owned())
                            .unwrap_or_default();
                        if enabled {
                            self.flash_info(format!("auto-merge on green: armed for {name}"));
                        } else {
                            self.flash_info(format!("auto-merge on green: off for {name}"));
                        }
                        // Optimistically flip the arm locally so the `⚡`
                        // glyph lands on this keypress, not one daemon
                        // round-trip later (invisible under output-heavy
                        // load, #1090). The daemon's echo confirms it — or,
                        // if the merge-on-green author gate declines, carries
                        // the real `false` back and the glyph clears.
                        self.sidebar.mark_auto_merge_on_green(
                            &lazybox_core::SessionKey::from(&workspace_key),
                            enabled,
                        );
                        cmds.push(IpcCommand::SetAutoMergeOnGreen {
                            session_key: lazybox_core::SessionKey::from(&workspace_key),
                            enabled,
                        });
                    }
                    Intent::Notice(msg) => self.flash_info(msg),
                    Intent::NoOp
                    | Intent::SpawnAgent { .. }
                    | Intent::SpawnShell { .. }
                    | Intent::MountReply { .. }
                    | Intent::MountNewWorkspaceInput { .. }
                    | Intent::MountAdoptPicker { .. }
                    | Intent::OpenEditor
                    | Intent::MergePr { .. }
                    | Intent::UpdateBranch { .. }
                    | Intent::SetTrackMain { .. }
                    | Intent::KillWorkspace { .. }
                    | Intent::Snooze { .. }
                    | Intent::Unsnooze { .. }
                    | Intent::MarkAllRead { .. }
                    | Intent::MarkActivitiesRead { .. }
                    | Intent::CollapseIntoPr { .. }
                    | Intent::MountHandoffPicker { .. } => {}
                }
            }
            Action::ToggleAutoFix => {
                let Some(workspace) = self.sidebar.selected_workspace().cloned() else {
                    return cmds;
                };
                if workspace.pr.is_none() {
                    return cmds;
                }
                let arm = if workspace.policies.any_auto_fix_armed() {
                    lazybox_core::PolicyArm::Disarm
                } else {
                    lazybox_core::PolicyArm::Arm
                };
                let name = crate::util::notice_slug(&workspace.name);
                match arm {
                    lazybox_core::PolicyArm::Arm if self.auto_fix_enabled => self.flash_info(
                        format!("auto-fix: armed for {name} (CI failures + conflicts)"),
                    ),
                    lazybox_core::PolicyArm::Arm => self
                        .flash_info(format!("auto-fix: armed for {name}, but disabled globally")),
                    lazybox_core::PolicyArm::Disarm => {
                        self.flash_info(format!("auto-fix: off for {name}"))
                    }
                    lazybox_core::PolicyArm::Default => unreachable!(),
                }
                let session_key = lazybox_core::SessionKey::from(&workspace.key);
                cmds.push(IpcCommand::SetAutoFixPolicies {
                    session_key,
                    ci: arm,
                    conflict: arm,
                });
            }
            Action::ToggleTrackMain => {
                let workspace = self.sidebar.selected_workspace().cloned();
                // Explicit variant list (no `_`): a new Intent variant is
                // triaged here at compile time, not silently dropped.
                use crate::intent::Intent;
                match crate::intent::resolve_toggle_track_main(workspace.as_ref()) {
                    Intent::SetTrackMain {
                        workspace_key,
                        enabled,
                    } => {
                        let name = workspace
                            .as_ref()
                            .map(|w| crate::util::notice_slug(&w.name).into_owned())
                            .unwrap_or_default();
                        if enabled {
                            self.flash_info(format!("track main: on for {name}"));
                        } else {
                            self.flash_info(format!("track main: off for {name}"));
                        }
                        cmds.push(IpcCommand::SetTrackMain {
                            session_key: lazybox_core::SessionKey::from(&workspace_key),
                            enabled,
                        });
                    }
                    Intent::Notice(msg) => self.flash_info(msg),
                    Intent::NoOp
                    | Intent::SpawnAgent { .. }
                    | Intent::SpawnShell { .. }
                    | Intent::MountReply { .. }
                    | Intent::MountNewWorkspaceInput { .. }
                    | Intent::MountAdoptPicker { .. }
                    | Intent::OpenEditor
                    | Intent::MergePr { .. }
                    | Intent::UpdateBranch { .. }
                    | Intent::SetAutoMergeOnGreen { .. }
                    | Intent::KillWorkspace { .. }
                    | Intent::Snooze { .. }
                    | Intent::Unsnooze { .. }
                    | Intent::MarkAllRead { .. }
                    | Intent::MarkActivitiesRead { .. }
                    | Intent::CollapseIntoPr { .. }
                    | Intent::MountHandoffPicker { .. } => {}
                }
            }
            Action::ManagePolicies => {
                // Unified automation-policies menu (issue #363). Surfaces
                // for any workspace carrying a PR or a GitHub issue; the
                // menu itself marks which policies apply to PRs vs issues.
                if let Some(ws) = self.sidebar.selected_workspace()
                    && (ws.pr.is_some() || !ws.gh_issues.is_empty())
                {
                    let ws_key = ws.key.clone();
                    self.mount_policy_picker(ws_key);
                }
            }
            Action::Refresh => {
                cmds.push(IpcCommand::Refresh);
                // Pre-arm the bg_poll indicator so the user gets
                // feedback on the keystroke — same as the
                // `Shift+R` handler did inline before.
                self.status
                    .note_poll_progress("github", "manual refresh requested");
                self.flash_hint("refreshing…");
                // Arm a one-shot ack so the next PollCompleted /
                // ProviderError surfaces a clear "✓ sync ok" or
                // "✗ sync failed" footer notice — silent
                // spinner-clears were being read as "did anything
                // happen?"
                self.pending_refresh_ack = true;
            }
            Action::ForceRedraw => {
                // Ctrl-L is the user saying "what I see is wrong — show
                // me the truth" (#1254). Repaint the host screen AND
                // re-request every visible terminal's authoritative
                // daemon replay: the repaint alone cannot fix a client
                // VT grid that parsed a torn stream. Deliberately NOT
                // inside `force_full_redraw` itself — resize and
                // focus-regain repaints happen constantly and must not
                // each fetch megabytes of replay ring.
                self.force_full_redraw();
                self.request_terminal_truth();
            }
            Action::OpenHelp => {
                self.mount_help_ask();
            }
            Action::OpenTour => {
                self.mount_tour();
            }
            Action::OpenSyncStatus => {
                self.mount_sync_status();
            }
            Action::OpenMessages => {
                self.mount_messages();
            }
            Action::OpenErrorInbox => {
                self.mount_error_inbox();
            }
            Action::OpenHopper => {
                self.mount_hopper();
            }
            Action::OpenSettings => {
                self.open_settings();
            }
            Action::OpenThemePicker => {
                self.mount_theme_picker();
            }
            Action::OpenSnippets => {
                self.mount_snippet_browser();
            }
            Action::JumpToWorkspace => {
                self.mount_jump_picker();
            }
            Action::JumpToAsking => {
                if self.sidebar.focus_next_asking_workspace() {
                    self.set_focus(PaneFocus::Sidebar);
                    self.redraw = true;
                }
            }
            Action::JumpToFailingCi => {
                if self.sidebar.focus_next_failing_ci_workspace() {
                    self.set_focus(PaneFocus::Sidebar);
                    self.redraw = true;
                } else {
                    self.flash_hint("no failing PRs");
                }
            }
            Action::JumpToLimited => {
                if self.sidebar.focus_next_limit_reached_workspace() {
                    self.set_focus(PaneFocus::Sidebar);
                    self.redraw = true;
                } else {
                    self.flash_hint("no rate-limited agents");
                }
            }
            Action::ResumeRateLimited => {
                cmds.extend(self.resume_rate_limited_agents());
            }
            Action::RecoverAgentCredit => {
                cmds.extend(self.recover_agent_credit(false));
            }
            Action::RecoverAllAgentCredit => {
                cmds.extend(self.recover_agent_credit(true));
            }
            Action::ToggleActivityPane => {
                if let Some(ws_key) = self.sidebar.selected_workspace().map(|w| w.key.clone()) {
                    self.activity_pane.cycle(
                        ws_key,
                        self.right.has_visible_content(),
                        self.ui_defaults.activity_pane_default,
                    );
                    // Don't strand focus on a pane we just collapsed.
                    self.enforce_pane_focus();
                    self.redraw = true;
                }
            }
            Action::ToggleFocusMode => {
                self.toggle_focus_mode();
            }
            Action::ConnectBox => {
                // Explicit connect/disconnect toggle (#1066). Connection is
                // first-class session state: this brings the box up (or
                // tears the link down) on demand, with the persistent
                // footer indicator showing progress. A no-op flash when no
                // `sandbox:` box is configured.
                if self.remote_control.is_none() {
                    // The box worker is wired at startup from the `sandbox:`
                    // config (#1112). No control channel means it was absent
                    // then — either nothing is configured (route the user
                    // into onboarding instead of a bare error) or a block was
                    // just written this session and needs a relaunch to take
                    // effect.
                    let configured = lazybox_config::Config::load()
                        .map(|c| !c.sandbox.is_empty())
                        .unwrap_or(false);
                    if configured {
                        self.flash_info(
                            "sandbox configured — restart lazybox to bring the box up with Shift-C",
                        );
                    } else {
                        self.start_sandbox_onboarding();
                    }
                } else if self.status.remote_conn.is_connected()
                    || self.status.remote_conn.is_busy()
                {
                    self.disconnect_remote();
                } else {
                    self.connect_remote();
                }
            }
            Action::StartAgent => {
                // Global "just start working" entry point. Unlike `n`
                // (which needs the sidebar cursor on a project), this
                // works from any pane: mount a project picker, then
                // the name input, then create+spawn. With a single
                // project the picker is skipped and we jump straight
                // to naming.
                self.start_agent_flow();
            }
            Action::Reply => {
                // Reply targets the focused workspace. Resolver
                // returns `Intent::MountReply` when a workspace is
                // selected; we mount the textarea modal. Fires from
                // both Sidebar and Right (catalog Section::Workspace
                // covers both focuses).
                let intent = crate::intent::resolve_reply(self.sidebar.selected_workspace());
                if let crate::intent::Intent::MountReply { workspace_key } = intent {
                    let session_key: lazybox_core::SessionKey = (&workspace_key).into();
                    self.mount_reply(session_key);
                }
            }
            Action::EditNotes => {
                // Notes attach to the focused workspace (any workspace,
                // even a session-less one). Section::Workspace, so this
                // fires from both Sidebar and Right focus.
                if let Some(ws) = self.sidebar.selected_workspace() {
                    let session_key: lazybox_core::SessionKey = (&ws.key).into();
                    self.mount_notes(session_key);
                }
            }
            Action::RequestReviewers => {
                if let Some(cmd) = self.begin_request_reviewers() {
                    cmds.push(cmd);
                }
            }
            Action::AddAssignees => {
                if let Some(ws) = self.sidebar.selected_workspace() {
                    // Assignment requires a provider assignable id — a
                    // PR, gh issue, or Linear issue with a node_id.
                    // Empty pre-PR workspaces don't qualify.
                    let has_target = ws.pr.as_ref().map(|p| p.node_id.is_some()).unwrap_or(false)
                        || ws
                            .gh_issues
                            .iter()
                            .chain(ws.linear_issues.iter())
                            .next()
                            .map(|i| i.node_id.is_some())
                            .unwrap_or(false);
                    if has_target {
                        let ws_key = ws.key.clone();
                        self.mount_add_assignees(ws_key);
                    }
                }
            }
            Action::ManageLabels => {
                // Labels require a `Labelable` node id — same as
                // assignees. Pre-PR scratch workspaces don't qualify.
                if let Some(ws) = self.sidebar.selected_workspace() {
                    let has_target = ws.pr.as_ref().map(|p| p.node_id.is_some()).unwrap_or(false)
                        || ws
                            .gh_issues
                            .first()
                            .map(|i| i.node_id.is_some())
                            .unwrap_or(false);
                    if !has_target {
                        self.flash_info("no PR / issue to label");
                        return cmds;
                    }
                    let ws_key = ws.key.clone();
                    // Two-step: ask the daemon for the repo's label
                    // set, then mount the picker when
                    // `IpcEvent::RepoLabels` arrives. Stash the
                    // workspace key so the event handler knows
                    // whether the response is still relevant.
                    self.awaiting_repo_labels = Some(ws_key.clone());
                    cmds.push(IpcCommand::FetchRepoLabels {
                        workspace_key: ws_key,
                    });
                    self.flash_hint("loading repo labels…");
                }
            }
            Action::OpenInBrowser => {
                // Read the primary task's URL and hand it to the
                // platform launcher. Surfaces a footer notice on
                // success / failure so the user knows whether the
                // browser actually came up — silent spawn failures
                // (no xdg-open on a headless box, etc.) would be
                // confusing otherwise.

                let Some(ws) = self.sidebar.selected_workspace() else {
                    return cmds;
                };
                let Some(url) = ws.primary_task().map(|t| t.url.clone()) else {
                    self.flash_info("no task URL on this workspace");
                    return cmds;
                };
                let browser = self.ui_defaults.browser.clone();
                match lazybox_tui_core::editors::open_url(&url, browser.as_deref()) {
                    Ok(()) => {
                        // open_url is fire-and-forget (the launcher is
                        // spawned, not waited on) — phrase as in-progress.
                        tracing::info!(%url, "opening workspace URL in browser");
                        self.flash_info(format!("opening {url}…"));
                    }
                    Err(e) => {
                        tracing::warn!(%url, "open_url failed: {e}");
                        self.flash(
                            format!("open failed: {e}"),
                            crate::realm::components::footer::NoticeSeverity::Retryable,
                        );
                    }
                }
            }
            Action::SyncWorkspace => {
                // Bulk (#899): re-poll every selected PR / issue at once;
                // rows with nothing to sync (no PR, no issue) are skipped
                // and counted.
                if self.bulk_active() {
                    return self.bulk_dispatch("synced", "nothing to sync", |ws| {
                        (ws.pr.is_some() || !ws.gh_issues.is_empty() || ws.repo_slug().is_some())
                            .then(|| IpcCommand::SyncWorkspace {
                                workspace_key: ws.key.clone(),
                            })
                    });
                }
                // Targeted re-poll of just this workspace's PR / issue —
                // cheaper than the global refresh when you're waiting on
                // one PR's CI. The daemon deep-fetches the entity and
                // upserts it, so the row's state + read markers update
                // without a full sweep. A repo-scoped workspace with no
                // entity yet still syncs: the daemon falls back to a
                // forced repo re-poll so `g s` never dead-ends on
                // "nothing to sync" just because no PR/issue landed here.
                let Some(ws) = self.sidebar.selected_workspace() else {
                    return cmds;
                };
                if ws.pr.is_none() && ws.gh_issues.is_empty() && ws.repo_slug().is_none() {
                    self.flash_info("nothing to sync on this workspace");
                    return cmds;
                }
                let workspace_key = ws.key.clone();
                cmds.push(IpcCommand::SyncWorkspace { workspace_key });
                self.flash_hint("syncing…");
            }
            Action::CyclePane => {
                // The keyboard path normally consumes the chord in
                // `handle_pane_key`'s guard arm (which owns the
                // live-terminal gating); this arm serves the other
                // surfaces (context menu, tests) with the same effect.
                self.cycle_pane_focus();
            }
            Action::FocusPaneRight => {
                // Sidebar `Right`/`l`: step focus into the pane on the
                // right. Skip the activity pane when it's hidden and go
                // straight to the terminal, mirroring the Tab-cycle skip
                // in `cycle_pane_focus`.
                self.set_focus(if self.activity_pane_visible() {
                    PaneFocus::Right
                } else {
                    PaneFocus::Terminals
                });
                self.redraw = true;
            }
            Action::FocusPaneLeft => {
                // Activity-pane `Left`/`h`: collapse an expanded row
                // first — that's the pane's own meaning — and only when
                // there's nothing left to collapse step focus back to
                // the sidebar, so arrow navigation is reversible without
                // clobbering the collapse gesture.
                if !self.right.collapse_focused_row() {
                    self.set_focus(PaneFocus::Sidebar);
                }
                self.redraw = true;
            }
            Action::ToggleMouseCapture => {
                self.toggle_mouse_capture();
            }
            Action::ActivityTop => {
                // `g` under Right focus: jump the activity cursor to
                // the first row. Catalog-dispatched so the vim
                // go-to-top reflex can never arm the Workspace `g *`
                // github leader from the activity pane (where a
                // reflexive `g g` used to silently toggle auto-merge).
                self.right.activity_cursor_top();
                self.redraw = true;
            }
            Action::ActivityBottom => {
                self.right.activity_cursor_bottom();
                self.redraw = true;
            }
            Action::OpenFilterMenu => {
                self.mount_filter_menu();
            }
            Action::CycleSort => {
                self.sidebar.cycle_sort();
            }
            Action::CycleMailbox => {
                self.sidebar.cycle_mailbox();
            }
            Action::OpenSearch => {
                self.sidebar.open_search();
            }
            Action::OpenGlobalSearch => {
                self.sidebar.open_global_search();
            }
            Action::ToggleRepoGroup => {
                // `Space` folds the most local tier at the cursor. A parent
                // ticket is the most local fold target: on a parent-ticket row
                // it folds that ticket's visible descendants (view-local).
                // Failing that, a Space header collapses the whole Space
                // (#860) and a repo header collapses its group. On a plain
                // workspace / session / kind row it is inert — a bare Space is
                // the most reflexively-pressed "neutral" key, so it must never
                // fold the group you're navigating inside (#1099). Collapse a
                // group by moving to its header or clicking the ▾ triangle.
                if self.sidebar.toggle_ticket_at_cursor() {
                    // handled
                } else if self.sidebar.cursor_on_space_header() {
                    self.sidebar.toggle_space_at_cursor();
                } else if self.sidebar.cursor_on_repo_header() {
                    self.sidebar.toggle_repo_at_cursor();
                }
            }
            Action::ToggleRepoPin => {
                if let Some((repo, pinned)) = self.sidebar.toggle_pin_at_cursor() {
                    let verb = if pinned { "pinned" } else { "unpinned" };
                    self.flash_info(format!("{verb} {repo}"));
                    self.redraw = true;
                }
            }
            Action::MoveGroupUp
            | Action::MoveGroupDown
            | Action::MoveGroupTop
            | Action::MoveGroupBottom => {
                use lazybox_tui_core::inbox::MoveDir;
                let dir = match action {
                    Action::MoveGroupUp => MoveDir::Up,
                    Action::MoveGroupDown => MoveDir::Down,
                    Action::MoveGroupTop => MoveDir::Top,
                    _ => MoveDir::Bottom,
                };
                match self.sidebar.move_group_at_cursor(dir) {
                    Some((what, name)) => {
                        self.flash_info(format!("moved {what} {name} {}", dir.label()));
                        self.redraw = true;
                    }
                    // Advise, never error: a lone group has nowhere to
                    // go; an empty sidebar has nothing to move.
                    None => self.flash_info("nothing to reorder here".to_string()),
                }
            }
            Action::ToggleFocusWorkspace => {
                if let Some((label, focused)) = self.sidebar.toggle_focus_at_cursor() {
                    let verb = if focused { "focused" } else { "unfocused" };
                    self.flash_info(format!("{verb} {label}"));
                    self.redraw = true;
                }
            }
            Action::SelectWorkspace => {
                // Toggle the cursor row's selection mark. The notice
                // names the running count and reminds that normal
                // actions now fan out over the whole selection (#932).
                // Visible-only count — the same number the header shows
                // and `resolve_targets` acts on (#786, #1243).
                if let Some(now_selected) = self.sidebar.toggle_broadcast_select() {
                    let n = self.sidebar.visible_broadcast_selected_count();
                    let verb = if now_selected {
                        "selected"
                    } else {
                        "deselected"
                    };
                    if n == 0 {
                        self.flash_info(format!("{verb} — selection empty"));
                    } else {
                        self.flash_info(format!(
                            "{verb} — {n} selected · actions apply to all · Esc clears"
                        ));
                    }
                    self.redraw = true;
                }
            }
            Action::BroadcastToSelected => {
                self.mount_broadcast_picker();
            }
            Action::SendToSession => {
                let workspace = self.sidebar.selected_workspace().cloned();
                let captured = workspace.as_ref().and_then(|workspace| {
                    let source_key = lazybox_core::SessionKey::from(&workspace.key);
                    self.terminals
                        .agent_terminal_for(&source_key)
                        .map(|terminal_id| {
                            self.terminals.visible_text(terminal_id).unwrap_or_default()
                        })
                });
                let intent = crate::intent::resolve_send_to_session(workspace.as_ref(), captured);
                cmds.extend(self.execute_dispatch_intent(intent, workspace.as_ref()));
            }
            Action::ConvertSession => {
                if self.conversion.is_some() {
                    self.flash_info("a session conversion is already in progress");
                    return cmds;
                }
                let Some(source_key) = self.sidebar.selected_workspace_key().cloned() else {
                    return cmds;
                };
                let focused_source = self.terminals.focused_terminal_id().filter(|terminal_id| {
                    self.terminals.terminal_is_agent(*terminal_id)
                        && self.terminals.session_key_for(*terminal_id) == Some(&source_key)
                });
                let Some(source_terminal) =
                    focused_source.or_else(|| self.terminals.agent_terminal_for(&source_key))
                else {
                    self.flash_info("no agent session here to convert");
                    return cmds;
                };
                let Some(agent) = self
                    .terminals
                    .terminal_agent_id(source_terminal)
                    .map(str::to_string)
                else {
                    self.flash_info("no agent session here to convert");
                    return cmds;
                };
                if !matches!(agent.as_str(), "claude" | "codex") {
                    self.flash_info(format!(
                        "{agent} does not support structured session handoffs"
                    ));
                    return cmds;
                }
                if self.terminals.terminal_is_on_main(source_terminal) {
                    self.flash_info("session conversion currently requires an isolated worktree");
                    return cmds;
                }
                let source_name = self
                    .sidebar
                    .workspace_by_key(&source_key)
                    .map(|workspace| workspace.name.clone())
                    .unwrap_or_else(|| source_key.to_string());
                self.mount_conversion_role_picker(super::ConversionDraft {
                    source_terminal,
                    source: source_key,
                    source_name,
                    agent,
                });
            }
            // Actions not yet handled here stay in the existing
            // handlers. As we migrate, the per-key match arms in
            // `handle_pane_key` and the pane wrappers get deleted
            // and the case lands here.
            other => {
                tracing::debug!(
                    "dispatch_action: {other:?} not yet migrated; falling back to legacy handler",
                );
            }
        }
        cmds
    }

    /// Fan a bulk `w w` / spawn / shell over the active `v` multi-select
    /// (#899). Builds the full spawn/inject plan up front; when it would
    /// start no new sessions (only injects into live agents) it runs
    /// immediately, otherwise it gates behind a `Confirm` that names the
    /// count — spawning N agents is heavy (#836) — snapshotting the plan
    /// so a poll under the modal can't change who starts.
    fn dispatch_bulk_agent(&mut self, op: BulkOp, model_alias: Option<String>) -> Vec<IpcCommand> {
        let targets = self.resolve_targets();
        let plan = self.build_bulk_agent_plan(&op, &targets, model_alias);
        let summary = bulk_agent_summary(&plan);
        if plan.spawned == 0 {
            // No heavy spawns — run the plan (injects, or nothing) now.
            if plan.injected > 0 {
                self.sidebar.clear_broadcast_selection();
            }
            if let Some(follow) = plan.follow {
                self.spawn_follow_to = Some(follow);
            }
            self.flash_info(summary);
            self.redraw = true;
            return self.run_bulk_agent_steps(plan.steps);
        }
        let remote = match &op {
            BulkOp::SpawnAgentRemote(_, remote) => Some(remote.as_str()),
            _ => None,
        };
        let prompt = bulk_agent_confirm_prompt(&plan, remote);
        self.set_modal_flow(super::ModalFlow::BulkSpawnConfirm {
            steps: plan.steps,
            summary,
            follow: plan.follow,
        });
        let modal = crate::realm::components::confirm::Confirm::new(&prompt).default_yes();
        self.mount_modal(super::Id::BulkSpawnConfirm, modal);
        Vec::new()
    }

    /// Resolve a broadcast's (snippet / free-text) payload into a
    /// [`BulkOp`] once. The snippet category is looked up here so the op
    /// carries everything `apply_one` needs per target.
    fn broadcast_op(&self, snippet_key: Option<&str>, body: &str) -> BulkOp {
        match snippet_key {
            Some(key) => BulkOp::Snippet {
                key: key.to_string(),
                category: self
                    .snippets
                    .get(key)
                    .map(|snippet| snippet.category.clone())
                    .unwrap_or_default(),
                body: body.to_string(),
            },
            None => BulkOp::Prompt {
                body: body.to_string(),
            },
        }
    }

    /// Fan a snippet / free-text broadcast over an explicit target set
    /// (#836, #1077) through the same [`Self::apply_one`] pipeline `w w`
    /// bulk start uses — a broadcast is just a `Snippet` / `Prompt` op.
    /// A live session is delivered to now; a session-less scoped row spawns
    /// the default agent seeded with the body, and because spawning is
    /// heavy the send gates behind a confirm first. `targets` is the set
    /// stashed when the flow mounted; the confirm stashes the op *inputs*
    /// (not the resolved steps) so a "yes" re-resolves each target's live
    /// session state — a target whose agent died under the modal recovers
    /// by re-spawning seeded, rather than firing at a now-dead terminal.
    pub(super) fn dispatch_broadcast_op(
        &mut self,
        targets: &[lazybox_core::SessionKey],
        snippet_key: Option<&str>,
        body: &str,
    ) -> Vec<IpcCommand> {
        let op = self.broadcast_op(snippet_key, body);
        let plan = self.build_bulk_agent_plan(&op, targets, None);
        if plan.spawned == 0 {
            // No heavy spawns — deliver now.
            return self.run_broadcast_plan(plan);
        }
        let agent = self.sidebar.default_agent().to_string();
        let prompt = if plan.spawned == 1 {
            format!(
                "1 selected workspace has no agent — start the default agent ({agent}) there and broadcast?"
            )
        } else {
            format!(
                "{} selected workspaces have no agent — start the default agent ({agent}) in each and broadcast?",
                plan.spawned
            )
        };
        self.set_modal_flow(super::ModalFlow::BroadcastConfirm {
            targets: targets.to_vec(),
            snippet_key: snippet_key.map(str::to_string),
            body: body.to_string(),
        });
        let modal = crate::realm::components::confirm::Confirm::new(&prompt).default_yes();
        self.mount_modal(super::Id::BroadcastConfirm, modal);
        Vec::new()
    }

    /// Run a confirmed broadcast (`Id::BroadcastConfirm` "yes"). Re-resolves
    /// the op against each target's *current* session state — the fixed
    /// target set can't change, but a target that lost (or gained) a live
    /// agent while the confirm was up is served correctly now, so a delivery
    /// never fires at a terminal that died under the modal (#1077 review).
    pub(super) fn run_broadcast_confirmed(
        &mut self,
        targets: &[lazybox_core::SessionKey],
        snippet_key: Option<&str>,
        body: &str,
    ) -> Vec<IpcCommand> {
        let op = self.broadcast_op(snippet_key, body);
        let plan = self.build_bulk_agent_plan(&op, targets, None);
        self.run_broadcast_plan(plan)
    }

    /// Materialize a broadcast plan: clear the multi-select when anything
    /// was delivered or started (but not when every target was skipped, so
    /// a retry keeps the marks), flash the outcome summary, and run the
    /// steps. Broadcast never follows focus (fire-and-stay), so
    /// `spawn_follow_to` is left untouched. Shared by the immediate and
    /// post-confirm paths so both materialize identically.
    fn run_broadcast_plan(&mut self, plan: BulkAgentPlan) -> Vec<IpcCommand> {
        let summary = broadcast_summary(&plan);
        if plan.injected > 0 || plan.spawned > 0 {
            self.sidebar.clear_broadcast_selection();
        }
        self.flash_info(summary);
        self.redraw = true;
        self.run_bulk_agent_steps(plan.steps)
    }

    /// Run a snapshotted bulk agent plan: emit each spawn and deliver
    /// each inject (the recap-mutating [`deliver_prompt`] fires here, at
    /// run time — never at plan-build time). Shared by the immediate
    /// (`spawned == 0`) path and the post-confirm handler so both
    /// materialize the plan identically.
    pub(super) fn run_bulk_agent_steps(
        &mut self,
        steps: Vec<super::BulkAgentStep>,
    ) -> Vec<IpcCommand> {
        let mut cmds = Vec::new();
        for step in steps {
            match step {
                super::BulkAgentStep::Spawn(cmd) => cmds.push(cmd),
                // Remote spawns never ride the local `cmds` flush — they go
                // straight to the box's client, and the row's `⇅` tag is
                // latched so the next local snapshot can't wipe it.
                super::BulkAgentStep::SpawnRemote { remote, key, cmd } => {
                    self.send_to_remote(&remote, cmd);
                    self.mark_remote_latched(key, remote);
                }
                super::BulkAgentStep::Inject { terminal_id, body } => {
                    self.deliver_prompt(
                        terminal_id,
                        true,
                        &body,
                        lazybox_ipc::PromptSource::Typed,
                        &mut cmds,
                    );
                }
                // A live shell has no paste debounce, so the encoded direct
                // write submits cleanly (free-text broadcast to a shell).
                super::BulkAgentStep::Write { terminal_id, body } => {
                    self.deliver_prompt(
                        terminal_id,
                        false,
                        &body,
                        lazybox_ipc::PromptSource::Typed,
                        &mut cmds,
                    );
                }
                // Snippet delivery rides the daemon's confirmed-delivery
                // command (agent + shell), leaving history / MRU behind the
                // daemon's ack.
                super::BulkAgentStep::DeliverSnippet {
                    terminal_id,
                    snippet_key,
                    category,
                    body,
                } => {
                    cmds.push(IpcCommand::DeliverSnippet {
                        terminal_id,
                        snippet_key,
                        category,
                        body,
                        submit: true,
                    });
                }
            }
        }
        cmds
    }

    /// Build the fan-out plan for a resolved [`BulkOp`] over an explicit
    /// target set, in the given order, by running [`Self::apply_one`] on
    /// each. The unified pipeline (#1077): callers resolve the target set
    /// once ([`Self::resolve_targets`] for the `v` multi-select, the
    /// stashed draft for a broadcast) and the op once, then this applies
    /// that same op to every target. Takes `&self` and produces only
    /// inert steps — nothing is delivered or recorded here, so a plan the
    /// user later cancels leaves no trace.
    fn build_bulk_agent_plan(
        &self,
        op: &BulkOp,
        targets: &[lazybox_core::SessionKey],
        model_alias: Option<String>,
    ) -> BulkAgentPlan {
        let mut plan = BulkAgentPlan::default();
        for key in targets {
            match self.apply_one(op, key, model_alias.as_deref()) {
                ApplyOutcome::Spawn { step, follow } => {
                    plan.steps.push(step);
                    plan.spawned += 1;
                    plan.follow.get_or_insert(follow);
                }
                ApplyOutcome::Live(step) => {
                    plan.steps.push(step);
                    plan.injected += 1;
                }
                ApplyOutcome::Skip(name) => plan.skipped.push(name),
                ApplyOutcome::Gone => {}
            }
        }
        plan
    }

    /// The single source of truth for "apply this op to this one
    /// workspace" (#1077) — the `apply_one` every fan-out shares. Given a
    /// resolved [`BulkOp`] and one target key, it produces the inert step
    /// to run (spawn / inject / write / snippet), a named skip, or `Gone`
    /// when the row vanished between mark and fire. Never touches the
    /// sidebar selection or the focused row: the target is the `key`
    /// argument alone, so no dispatchable action re-derives its target
    /// from `selected_workspace()`.
    ///
    /// A workspace running *several* agents (`WorkTarget::Choose`, #418)
    /// can't raise its per-row chooser mid-fan-out, so bulk work targets
    /// its first live agent — the deliberate "resolve per target, don't
    /// prompt N times" bulk trade-off.
    fn apply_one(
        &self,
        op: &BulkOp,
        key: &lazybox_core::SessionKey,
        model_alias: Option<&str>,
    ) -> ApplyOutcome {
        use crate::components::sidebar::WorkTarget;
        let Some(ws) = self.sidebar.workspace_by_key(key) else {
            return ApplyOutcome::Gone;
        };
        let name = crate::util::notice_slug(&ws.name).into_owned();
        let spawnable = ws.worktree_scope().is_some();
        let default_agent = self.sidebar.default_agent().to_string();
        let model_alias = model_alias.map(str::to_string);
        // A session-less but spawnable target seeds the default agent with
        // the delivered body (#836) — shared by the snippet and free-text
        // ops so both auto-start identically. A snippet seed carries its
        // identity so the daemon records the same MRU / sent history the
        // live-terminal delivery records (#1215).
        let seed_spawn =
            |body: &str, snippet: Option<lazybox_ipc::SnippetRef>| ApplyOutcome::Spawn {
                step: super::BulkAgentStep::Spawn(bulk_spawn_command(
                    key.clone(),
                    lazybox_ipc::TerminalKind::Agent(default_agent.clone()),
                    Some(body.to_string()),
                    snippet,
                    None,
                )),
                follow: key.clone(),
            };
        match op {
            // Plain spawns always start a fresh session — a repo-less,
            // project-less row (a Slack DM, scratch) has nothing to
            // spawn into (#836), so it's skipped and named.
            BulkOp::SpawnShell | BulkOp::SpawnAgent(_) | BulkOp::SpawnAgentRemote(..) => {
                if !spawnable {
                    return ApplyOutcome::Skip(name);
                }
                // Per-project sandbox opt-out (#1066): a remote fan-out
                // resolves each target against its repo's box and skips
                // (naming it) any workspace whose repo set `sandbox: false`,
                // so a disabled project is never spawned on the box.
                let remote_target = if let BulkOp::SpawnAgentRemote(..) = op {
                    match self.remote_for_repo(ws.repo_slug().as_deref()) {
                        Some(remote) => Some(remote.to_string()),
                        None => return ApplyOutcome::Skip(name),
                    }
                } else {
                    None
                };
                let kind = match op {
                    BulkOp::SpawnShell => lazybox_ipc::TerminalKind::Shell,
                    BulkOp::SpawnAgent(agent) | BulkOp::SpawnAgentRemote(agent, _) => {
                        lazybox_ipc::TerminalKind::Agent(agent.clone())
                    }
                    _ => unreachable!(),
                };
                let cmd = bulk_spawn_command(key.clone(), kind, None, None, model_alias);
                let step = match remote_target {
                    Some(remote) => super::BulkAgentStep::SpawnRemote {
                        remote,
                        key: key.clone(),
                        cmd,
                    },
                    None => super::BulkAgentStep::Spawn(cmd),
                };
                ApplyOutcome::Spawn {
                    step,
                    follow: key.clone(),
                }
            }
            // Contextual work: continue a live agent, else start one.
            BulkOp::Work | BulkOp::WorkWith(_) => {
                let target = match op {
                    BulkOp::WorkWith(agent) => self.sidebar.work_target_for_agent(key, agent),
                    _ => self.sidebar.work_target(key, &default_agent),
                };
                let agent_id = match &target {
                    WorkTarget::Spawn(agent) => agent.clone(),
                    WorkTarget::Running(running) => running.agent_id.clone(),
                    WorkTarget::Choose(targets) => targets
                        .first()
                        .map(|t| t.agent_id.clone())
                        .unwrap_or_else(|| default_agent.clone()),
                };
                let running_terminal = match &target {
                    WorkTarget::Running(running) => Some(running.terminal_id),
                    WorkTarget::Choose(targets) => targets.first().map(|t| t.terminal_id),
                    WorkTarget::Spawn(_) => None,
                };
                let intent = crate::intent::resolve_work(
                    Some(ws),
                    &[],
                    &agent_id,
                    self.sidebar.conventions(),
                );
                match intent {
                    crate::intent::Intent::SpawnAgent {
                        workspace_key: _,
                        agent_id,
                        prompt,
                    } => match (running_terminal, prompt) {
                        // A live agent + fresh instructions → inject.
                        (Some(terminal_id), Some(body)) => {
                            ApplyOutcome::Live(super::BulkAgentStep::Inject { terminal_id, body })
                        }
                        // A live agent but nothing new to say (a scratch row
                        // already being worked) → leave it be.
                        (Some(_), None) => ApplyOutcome::Skip(name),
                        // No live agent → spawn one with the prompt.
                        (None, body) => ApplyOutcome::Spawn {
                            step: super::BulkAgentStep::Spawn(bulk_spawn_command(
                                key.clone(),
                                lazybox_ipc::TerminalKind::Agent(agent_id),
                                body,
                                None,
                                model_alias,
                            )),
                            follow: key.clone(),
                        },
                    },
                    // Merged / closed workspace steers to cleanup.
                    _ => ApplyOutcome::Skip(name),
                }
            }
            // Snippet delivery: a live session gets the confirmed-delivery
            // command; a session-less scoped row auto-starts seeded (#836);
            // a repo-less row is skipped.
            BulkOp::Snippet {
                key: snippet_key,
                category,
                body,
            } => match self.sidebar.broadcast_terminal(key) {
                Some((terminal_id, _)) => {
                    ApplyOutcome::Live(super::BulkAgentStep::DeliverSnippet {
                        terminal_id,
                        snippet_key: snippet_key.clone(),
                        category: category.clone(),
                        body: body.clone(),
                    })
                }
                None if spawnable => seed_spawn(
                    body,
                    Some(lazybox_ipc::SnippetRef {
                        key: snippet_key.clone(),
                        category: category.clone(),
                    }),
                ),
                None => ApplyOutcome::Skip(name),
            },
            // Free-text delivery: a live agent gets the settle-gated inject,
            // a live shell the encoded write; same auto-start / skip tail.
            BulkOp::Prompt { body } => match self.sidebar.broadcast_terminal(key) {
                Some((terminal_id, true)) => ApplyOutcome::Live(super::BulkAgentStep::Inject {
                    terminal_id,
                    body: body.clone(),
                }),
                Some((terminal_id, false)) => ApplyOutcome::Live(super::BulkAgentStep::Write {
                    terminal_id,
                    body: body.clone(),
                }),
                None if spawnable => seed_spawn(body, None),
                None => ApplyOutcome::Skip(name),
            },
        }
    }

    /// Resolve and fire `w w` (or a `w S` tier chord) on the selected
    /// workspace (issue #418). Shared by `Work` and `WorkTier` so both
    /// pick the same agent before layering a tier on top:
    ///
    /// - one live conversation → work targets its exact terminal;
    /// - several live conversations → mount the chooser instead of
    ///   silently guessing, including when several use the same agent;
    /// - none → fresh spawn of the configured default.
    /// Carry out the merge-conflict resolve the user accepted at the
    /// `g m` resolve prompt (#947). Focus the stashed target so
    /// `dispatch_work` resolves against the right PR (a daemon event
    /// may have drifted the cursor while the prompt was up), clear any
    /// activity selection so the FixConflict classification wins over
    /// AddressComments, spawn/attach the agent with the
    /// conflict-resolution prompt, and re-sync the PR so a stale
    /// CONFLICT can't strand the user and the pill reflects reality
    /// (ties #144).
    pub(crate) fn dispatch_conflict_resolve(
        &mut self,
        workspace: &lazybox_core::SessionKey,
    ) -> Vec<IpcCommand> {
        let mut cmds = Vec::new();
        let Some(workspace_key) = self
            .sidebar
            .workspace_by_key(workspace)
            .map(|ws| ws.key.clone())
        else {
            self.flash_info("workspace is gone — nothing to resolve");
            return cmds;
        };
        if !self.sidebar.reveal_workspace_key(workspace) {
            self.flash_info("workspace is gone — nothing to resolve");
            return cmds;
        }
        self.right.clear_activity_selection();
        self.dispatch_work(None, None, &mut cmds);
        cmds.push(IpcCommand::SyncWorkspace { workspace_key });
        cmds
    }

    fn dispatch_work(
        &mut self,
        session_id: Option<lazybox_core::SessionId>,
        model_alias: Option<String>,
        cmds: &mut Vec<IpcCommand>,
    ) {
        use crate::components::sidebar::WorkTarget;
        let default_agent = self.sidebar.default_agent().to_string();
        let target = match self.sidebar.selected_workspace_key().cloned() {
            Some(sk) => self.sidebar.work_target(&sk, &default_agent),
            None => WorkTarget::Spawn(default_agent),
        };
        self.dispatch_work_target(target, session_id, model_alias, cmds);
    }

    /// Resolve a scoped `w <agent>` without letting another running
    /// agent kind participate in the choice.
    fn dispatch_work_with(
        &mut self,
        agent_id: &str,
        session_id: Option<lazybox_core::SessionId>,
        model_alias: Option<String>,
        cmds: &mut Vec<IpcCommand>,
    ) {
        use crate::components::sidebar::WorkTarget;
        let target = match self.sidebar.selected_workspace_key().cloned() {
            Some(sk) => self.sidebar.work_target_for_agent(&sk, agent_id),
            None => WorkTarget::Spawn(agent_id.to_string()),
        };
        self.dispatch_work_target(target, session_id, model_alias, cmds);
    }

    fn dispatch_work_target(
        &mut self,
        target: crate::components::sidebar::WorkTarget,
        session_id: Option<lazybox_core::SessionId>,
        model_alias: Option<String>,
        cmds: &mut Vec<IpcCommand>,
    ) {
        use crate::components::sidebar::WorkTarget;
        match target {
            WorkTarget::Spawn(agent_id) => {
                self.push_work_command(&agent_id, None, session_id, model_alias, cmds);
            }
            WorkTarget::Running(target) => {
                self.push_work_command(
                    &target.agent_id,
                    Some(target.terminal_id),
                    session_id,
                    model_alias,
                    cmds,
                );
            }
            WorkTarget::Choose(targets) => {
                self.mount_work_agent_picker(targets, session_id, model_alias);
            }
        }
    }

    /// Resolve a "work on this" command for `agent_id` and queue it.
    /// Shared by `w w` ([`lazybox_tui_core::action::Action::Work`]), the scoped `w c` / `w x`
    /// chords ([`lazybox_tui_core::action::Action::WorkWith`]), and the which-agent picker's pick
    /// (`choice_picked_inner`, issue #418): all build the same
    /// contextual prompt via [`crate::intent::resolve_work`] and differ
    /// only in how the target conversation is chosen. An exact running
    /// terminal receives an inject; otherwise the command remains a
    /// spawn.
    ///
    /// The activity selection lives in the right pane, but `w` must honor
    /// it from any focus — reading it here is sound because `set_workspace`
    /// clears the selection whenever the workspace key changes, so the
    /// right pane's indices always belong to the selected workspace.
    pub(super) fn push_work_command(
        &mut self,
        agent_id: &str,
        terminal_id: Option<lazybox_ipc::TerminalId>,
        session_id: Option<lazybox_core::SessionId>,
        model_alias: Option<String>,
        cmds: &mut Vec<IpcCommand>,
    ) {
        let workspace = self.sidebar.selected_workspace();
        let selected = self.right.selected_activity_indices();
        let intent =
            crate::intent::resolve_work(workspace, &selected, agent_id, self.sidebar.conventions());
        match intent {
            crate::intent::Intent::SpawnAgent {
                workspace_key,
                agent_id,
                prompt,
            } => {
                // Pin the follow target to the workspace `w` fired on so the
                // spawned agent's terminal is what focus lands on, even if a
                // slow first-time worktree provision lets the cursor wander
                // before the `TerminalSpawned` arrives (consumed in the
                // spawn-event handler).
                self.spawn_follow_to = Some((&workspace_key).into());
                let spawn = IpcCommand::Spawn {
                    session_key: (&workspace_key).into(),
                    session_id,
                    client_request_id: None,
                    kind: lazybox_ipc::TerminalKind::Agent(agent_id),
                    cwd: None,
                    initial_prompt: prompt,
                    initial_snippet: None,
                    on_main: false,
                    model_alias,
                    access: lazybox_ipc::AgentRunAccess::Default,
                    // `w` / `w w` continue an existing conversation when one is
                    // live (reuse/inject) — never force a duplicate.
                    force_new: false,
                };
                let command = match terminal_id {
                    Some(terminal_id) => self.rewrite_spawn_to_terminal(spawn, terminal_id),
                    None => spawn,
                };
                cmds.push(command);
                self.right.clear_activity_selection();
            }
            // A merged/closed workspace steers to cleanup: surface the
            // "press x x to archive" notice rather than provisioning a
            // worktree for finished work (issue #557).
            crate::intent::Intent::Notice(msg) => self.flash_hint(msg),
            _ => {
                // Nothing actionable (no workspace selected). Surface that
                // instead of silently swallowing the keystroke.
                self.flash_hint("nothing to work on here");
            }
        }
    }

    /// Enter or leave focus mode (issue #156). Entering requires a
    /// live terminal to maximize — focus mode over an empty stack
    /// would show a blank screen — so we flash a hint and stay put
    /// when there's nothing to focus. Entering pins focus to the
    /// terminal; leaving keeps it there so the user lands back in the
    /// three-pane view still driving the same agent.
    pub(super) fn toggle_focus_mode(&mut self) {
        if self.focus_mode {
            self.focus_mode = false;
            self.focus_zoom = false;
            self.redraw = true;
            return;
        }
        if self.terminals.is_empty() {
            self.flash_hint("no agent terminal to focus");
            return;
        }
        // The workspace focus mode is about to show: entering from the
        // sidebar, the post-dispatch `sync_panes` makes the CURSOR
        // workspace the active session (that's what `.` means there);
        // from the terminal (`]]f`) the active session already is it.
        // Pane 1 anchors on that eventual state, not the stale active
        // session (#1258).
        let anchor = if self.focus == PaneFocus::Sidebar {
            self.sidebar
                .selected_workspace_key()
                .cloned()
                .or_else(|| self.terminals.active_session().cloned())
        } else {
            self.terminals.active_session().cloned()
        };
        self.focus_mode = true;
        self.set_focus(PaneFocus::Terminals);
        // Multi-pane layouts (#1258) re-derive their roster on every
        // entry so pane 1 is the workspace being focused *now*, with
        // the starred roster behind it.
        self.focus_zoom = false;
        self.focus_pane = 0;
        if self.focus_layout != lazybox_config::FocusLayout::Single {
            self.populate_focus_panes_from(anchor);
        }
        self.redraw = true;
    }

    /// Whether the pane-level `]]` chords (#1258) are live: focus mode
    /// with a multi-pane layout. Pane zoom deliberately doesn't turn
    /// this off — zoom is render-only, so arrows keep moving pane focus
    /// (and the zoom follows) underneath it.
    pub(super) fn focus_multi_pane_active(&self) -> bool {
        self.focus_mode && self.focus_layout != lazybox_config::FocusLayout::Single
    }

    /// Fill the multi-pane focus roster (#1258): pane 1 = the current
    /// workspace, panes 2..N = the next starred workspaces in sidebar
    /// order (dedup, only those with a live terminal to show), then
    /// most-recently-active agent workspaces, then `None` (rendered as
    /// a dim placeholder naming the star action). Always fills all
    /// four slots so cycling deeper into `Grid` keeps pane continuity.
    pub(super) fn populate_focus_panes(&mut self) {
        let anchor = self.terminals.active_session().cloned();
        self.populate_focus_panes_from(anchor);
    }

    /// [`Self::populate_focus_panes`] with an explicit pane-1 anchor —
    /// the workspace that is (or is about to become) current. Anchors
    /// without a live terminal fall through to the roster like any
    /// other workspace.
    pub(super) fn populate_focus_panes_from(&mut self, anchor: Option<lazybox_core::SessionKey>) {
        let mut chosen: Vec<lazybox_core::SessionKey> = Vec::new();
        if let Some(cur) = anchor
            && self.terminals.terminal_count_for(&cur) > 0
        {
            chosen.push(cur);
        }
        for key in self.sidebar.numbered_workspace_keys() {
            if chosen.len() >= 4 {
                break;
            }
            if !chosen.contains(&key) && self.terminals.terminal_count_for(&key) > 0 {
                chosen.push(key);
            }
        }
        for key in self.terminals.recent_agent_sessions() {
            if chosen.len() >= 4 {
                break;
            }
            if !chosen.contains(&key) {
                chosen.push(key);
            }
        }
        let mut slots: [Option<lazybox_core::SessionKey>; 4] = [None, None, None, None];
        for (slot, key) in slots.iter_mut().zip(chosen) {
            *slot = Some(key);
        }
        self.focus_pane_slots = slots;
    }

    /// Cycle the focus-mode layout (`]]v`, #1258): Single → SplitV →
    /// SplitH → Grid → Single, persisted as `ui.focus_layout` so the
    /// choice survives restarts and reaches attach clients through
    /// `apply_client_config`. Only acts inside focus mode — outside it
    /// the layout has nothing to show, so flash instead of silently
    /// flipping a persisted setting.
    pub(super) fn cycle_focus_layout(&mut self) {
        if !self.focus_mode {
            self.flash_hint("focus layout applies in focus mode — ]]f first");
            return;
        }
        let was = self.focus_layout;
        let next = was.next();
        self.focus_layout = next;
        self.focus_zoom = false;
        // Cycling out of Single starts a fresh multi-pane journey:
        // derive the roster from the workspace being viewed right now.
        // Deeper cycles (SplitV → SplitH → Grid) keep the assignments
        // so panes don't reshuffle underfoot.
        if was == lazybox_config::FocusLayout::Single && next != lazybox_config::FocusLayout::Single
        {
            self.populate_focus_panes();
            self.focus_pane = 0;
        }
        if self.focus_pane >= next.pane_count() {
            // The layout shrank under the focused pane. Keep the
            // workspace the user was driving on screen: its slot moves
            // to pane 1 rather than the view teleporting back.
            self.focus_pane_slots.swap(0, self.focus_pane);
            self.set_focus_pane(0);
        }
        // Runtime flip first; the save failure is surfaced, not rolled
        // back (same contract as `]]t` / `ui.terminal_new_layout`).
        match lazybox_config::Config::save_with(|c| c.ui.focus_layout = next) {
            Ok(()) => self.flash_info(format!("focus layout: {}", next.label())),
            Err(e) => self.flash_info(format!(
                "focus layout: {} (couldn't save: {e})",
                next.label()
            )),
        }
        self.redraw = true;
    }

    /// Focus the `idx`-th workspace pane (#1258). A pane with a
    /// workspace routes keyboard input there by making its session the
    /// active one — the same sidebar-focus + `sync_panes` path the
    /// `]]<digit>` jump uses, so the event header, activity scoping,
    /// and the PTY write path all follow. A placeholder pane still
    /// takes the accent border (so `]]<digit>` can retarget it) but
    /// leaves the active session — and therefore typing — where it was.
    pub(super) fn set_focus_pane(&mut self, idx: usize) {
        self.focus_pane = idx;
        if let Some(key) = self.focus_pane_slots.get(idx).cloned().flatten()
            && self.sidebar.focus_workspace_key(&key)
        {
            self.set_focus(PaneFocus::Terminals);
            self.sync_panes();
        }
        self.redraw = true;
    }

    /// Move pane focus one step (`]]<arrow>` in a multi-pane focus
    /// layout, #1258). Panes-first by design: in a multi-pane layout
    /// the arrows address workspace panes exclusively — each pane shows
    /// a single terminal, so intra-workspace tile motion has no visible
    /// target there and stays a `Single`-layout behavior. Motion clamps
    /// at the edges (no wrap).
    pub(super) fn move_focus_pane(&mut self, dir: lazybox_core::TileDirection) {
        let visible = self.focus_layout.pane_count();
        let next = crate::realm::layout::focus_pane_move(self.focus_layout, self.focus_pane, dir)
            .min(visible.saturating_sub(1));
        if next != self.focus_pane {
            self.set_focus_pane(next);
        }
    }

    /// Retarget the FOCUSED pane to the Nth starred workspace
    /// (`]]<digit>` in a multi-pane focus layout, #1258; 1-based like
    /// the sidebar badges). If that workspace already occupies another
    /// pane the two panes swap — the alternative (two panes sharing one
    /// terminal) would render one VT into two different rects and
    /// flip-flop its PTY size every frame.
    pub(super) fn retarget_focus_pane(&mut self, n: usize) {
        let roster = self.sidebar.numbered_workspace_keys();
        let Some(target) = roster.get(n.saturating_sub(1)).cloned() else {
            self.flash_hint(format!("no focused workspace #{n} — star one with focus"));
            return;
        };
        // Search ALL four slots, not just the visible ones: a duplicate
        // parked in a hidden slot would surface as two panes sharing
        // one terminal on the next cycle into Grid.
        let already = self
            .focus_pane_slots
            .iter()
            .position(|s| s.as_ref() == Some(&target));
        match already {
            Some(other) if other != self.focus_pane => {
                self.focus_pane_slots.swap(self.focus_pane, other);
            }
            Some(_) => {}
            None => self.focus_pane_slots[self.focus_pane] = Some(target),
        }
        // Re-focus the (possibly new) workspace under the focused pane
        // so input routing and the event header follow the retarget.
        self.set_focus_pane(self.focus_pane);
    }

    /// Toggle pane-level zoom (`]]z` in a multi-pane focus layout,
    /// #1258): render the focused pane full-screen — exactly the
    /// `Single` layout — and back. Mirrors the tile zoom's feedback
    /// contract: zooming in flashes the restore chord, restoring is its
    /// own feedback.
    pub(super) fn toggle_focus_pane_zoom(&mut self) {
        self.focus_zoom = !self.focus_zoom;
        if self.focus_zoom {
            self.flash_hint("zoomed pane — ]]z to restore");
        }
        self.redraw = true;
    }

    /// Toggle tmux-style zoom (`]]z`, #1057) of the focused tile: the
    /// "read one closely, then zoom out to the grid" motion. Only the
    /// multi-tile Splits grid can zoom — flash a hint otherwise so the
    /// chord isn't a silent no-op. Restoring the grid is its own feedback,
    /// so only the zoom-in and the nothing-to-zoom cases flash.
    pub(super) fn toggle_terminal_zoom(&mut self) {
        match self.terminals.toggle_zoom() {
            Some(true) => self.flash_hint("zoomed tile — ]]z to restore"),
            Some(false) => {}
            None => self.flash_hint("nothing to zoom — split first (]]| / ]]-)"),
        }
        self.redraw = true;
    }

    /// Switch the displayed terminal to the Nth numbered (focused)
    /// workspace, counting in sidebar (top-down) order — the
    /// deterministic `]]<digit>` jump. `n` is 1-based, matching the
    /// number badge on the sidebar row and the roster in the `]]` leader
    /// popup. Keeps focus on the terminal so it works seamlessly inside
    /// focus mode; flashes when no focused workspace holds that slot
    /// (nothing starred, or fewer than `n`).
    pub(super) fn jump_to_numbered_workspace(&mut self, n: usize) {
        if self.sidebar.focus_nth_numbered_workspace(n) {
            self.set_focus(PaneFocus::Terminals);
            self.sync_panes();
            self.redraw = true;
        } else {
            self.flash_hint(format!("no focused workspace #{n} — star one with focus"));
        }
    }

    /// Leave the terminal pane back to the sidebar. Exits focus mode
    /// if it was on — the sidebar is hidden there, so returning to it
    /// must restore the normal three-pane layout.
    pub(super) fn leave_terminal_to_sidebar(&mut self) {
        self.focus_mode = false;
        self.set_focus(PaneFocus::Sidebar);
    }
}
