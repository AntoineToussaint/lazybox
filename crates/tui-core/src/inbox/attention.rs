//! Pure visibility + attention helpers for the inbox: which mailbox a
//! workspace belongs in, and which attention signals it is currently
//! raising. The ratatui-styled pills that *render* these signals stay
//! in the TUI's `sidebar::pills`; only the client-free producers live
//! here so the desktop client scores attention identically.

use super::Mailbox;
use lazybox_core::{SessionKey, Workspace};
use std::collections::HashMap;

/// How long a freshly-merged/closed PR stays visible in the Inbox
/// before falling into the Inactive mailbox. The point: when a PR
/// merges between polls, the gh-provider's recently-merged sweep
/// catches it and updates `state=Merged`. Without this grace
/// window the row would IMMEDIATELY disappear from the Inbox view
/// the user was looking at — they'd never see the MERGED pill.
///
/// 30 minutes is enough to give lazybox a poll cycle (or two) to
/// surface the state transition while the user is still around,
/// without permanently cluttering Inbox with completed work.
pub const INACTIVE_GRACE: chrono::Duration = chrono::Duration::minutes(30);

/// Pure predicate: does `workspace` belong in `mailbox` right now?
///
/// Single source of truth for the inbox / inactive / snoozed
/// classification. The body used to live inline in
/// `recompute_visible_inner`, where each branch was hand-rolled
/// and the snoozed-wins-over-merged subtlety wasn't covered by
/// any test. Pulling it out lets the test file exercise every
/// (workspace state, mailbox) cell directly.
///
/// Rules (snooze always wins — a snoozed workspace appears ONLY
/// in the Snoozed mailbox, never leaks into Inbox / Inactive):
///
/// - **Inbox**: not snoozed AND
///   (`show_inactive_in_inbox` OR the primary task is alive —
///   open / draft / in-progress / in-review). Empty workspaces
///   (no primary task at all) show in Inbox so the user can act
///   on them.
/// - **Inactive**: not snoozed AND primary task is `Merged` /
///   `Closed`.
/// - **Snoozed**: workspace is snoozed.
pub fn mailbox_membership(
    workspace: &Workspace,
    mailbox: Mailbox,
    now: chrono::DateTime<chrono::Utc>,
    show_inactive_in_inbox: bool,
) -> bool {
    let snoozed = workspace.is_snoozed(now);
    let hopper_completed = workspace
        .hopper
        .is_some_and(|hopper| hopper.completed_at.is_some());
    // "Recently inactivated" = task is Merged/Closed AND it reached
    // that state within the grace window. Such workspaces appear in
    // BOTH Inbox (so the user sees the MERGED/CLOSED transition) and
    // Inactive (so they're already in their permanent home).
    //
    // The clock is `closed_at` (the merge/close moment), NOT
    // `updated_at`: GitHub bumps `updated_at` on ANY later activity —
    // post-merge comments, branch deletion, linked-issue closure,
    // deploy statuses. Keying off `updated_at` let a merged PR in an
    // active repo keep resetting its own grace window, so it never
    // fell out of the Inbox (issue #96). `closed_at` only moves once.
    // Falls back to `updated_at` for records that predate the field.
    let recently_inactivated = workspace
        .primary_task()
        .map(|t| {
            matches!(
                t.state,
                lazybox_core::TaskState::Merged | lazybox_core::TaskState::Closed
            ) && (now - t.closed_at.unwrap_or(t.updated_at)) < INACTIVE_GRACE
        })
        .unwrap_or(false);
    match mailbox {
        Mailbox::Snoozed => snoozed,
        Mailbox::Inbox => {
            if snoozed {
                return false;
            }
            if hopper_completed {
                return show_inactive_in_inbox;
            }
            if show_inactive_in_inbox {
                return true;
            }
            match workspace.primary_task() {
                Some(t) => {
                    let is_terminal = matches!(
                        t.state,
                        lazybox_core::TaskState::Merged | lazybox_core::TaskState::Closed
                    );
                    !is_terminal || recently_inactivated
                }
                None => true,
            }
        }
        Mailbox::Inactive => {
            if snoozed {
                return false;
            }
            if hopper_completed {
                return true;
            }
            matches!(
                workspace.primary_task().map(|t| t.state),
                Some(lazybox_core::TaskState::Merged) | Some(lazybox_core::TaskState::Closed)
            )
        }
    }
}

/// One reason a workspace might want the user's attention. Single
/// vocabulary used by `workspace_attention_signals` (pure producer),
/// `workspace_needs_attention` (gated by config), and the per-
/// signal header counters (`Unread`/`AgentAsking`/`CiFailing`/…).
///
/// Adding a new signal means: add a variant here, add a producer
/// branch in `workspace_attention_signals`, add a config flag, and
/// — because the gate match below is exhaustive — the compiler
/// catches the missing wiring.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AttentionSignal {
    Unread,
    AgentAsking,
    CiFailing,
    ReviewPending,
    Mentioned,
}

/// Pure: every attention signal currently active for `w`. Order is
/// stable (matches the producer below) so callers that want
/// priority-aware behavior can read the first hit.
///
/// **Single source of truth.** The per-repo `needs attention`
/// counter, the header signal totals (`? N input`, `N CI`,
/// `N review`), and the row pill all derive from this one
/// producer. Before unification, each consumer had its own ad-hoc
/// check: `review_pending_count` counted "reviewers requested"
/// (even without ChangesRequested), `workspace_needs_attention`
/// didn't — so a repo with reviewers-only PRs lit the header
/// counter but not the repo badge.
pub fn workspace_attention_signals(
    w: &Workspace,
    agents: &HashMap<SessionKey, lazybox_ipc::AgentState>,
) -> Vec<AttentionSignal> {
    let mut out = Vec::new();
    if w.unread_count() > 0 {
        out.push(AttentionSignal::Unread);
    }
    // AgentAsking signal: source of truth is the sidebar-local
    // `agents` state map (driven by `Event::AgentState` deltas).
    // NOT `w.sessions[i].state` — that gets blown away every poll
    // when `WorkspaceUpserted` re-loads from the persisted store.
    if crate::agent_attention::workspace_is_asking(w, agents) {
        out.push(AttentionSignal::AgentAsking);
    }
    if let Some(t) = w.primary_task() {
        if matches!(
            t.ci,
            lazybox_core::CiStatus::Failure | lazybox_core::CiStatus::Mixed
        ) {
            out.push(AttentionSignal::CiFailing);
        }
        // ReviewPending unifies: explicit ReviewStatus + reviewers
        // requested. The previous split (header counter had the
        // `reviewers.is_empty()` extra, attention badge didn't) led
        // to "1 review" in the header next to a repo header with no
        // attention dot — confusing.
        if matches!(
            t.review,
            lazybox_core::ReviewStatus::Pending | lazybox_core::ReviewStatus::ChangesRequested,
        ) || !t.reviewers.is_empty()
        {
            out.push(AttentionSignal::ReviewPending);
        }
        if matches!(t.role, lazybox_core::TaskRole::Mentioned) {
            out.push(AttentionSignal::Mentioned);
        }
    }
    out
}

/// Is `signal` enabled in the user's attention config? Exhaustive
/// match so a new `AttentionSignal` variant fails to compile until
/// it's wired up here AND in `AttentionConfig`.
pub fn attention_gate(signal: AttentionSignal, cfg: &lazybox_config::AttentionConfig) -> bool {
    match signal {
        AttentionSignal::Unread => cfg.unread,
        AttentionSignal::AgentAsking => cfg.agent_asking,
        AttentionSignal::CiFailing => cfg.ci_failing,
        AttentionSignal::ReviewPending => cfg.review_pending,
        AttentionSignal::Mentioned => cfg.mentioned,
    }
}

pub fn workspace_needs_attention(
    w: &Workspace,
    cfg: &lazybox_config::AttentionConfig,
    agents: &HashMap<SessionKey, lazybox_ipc::AgentState>,
) -> bool {
    workspace_attention_signals(w, agents)
        .iter()
        .any(|s| attention_gate(*s, cfg))
}

/// Direct-address punch-through for the source-attention ladder
/// (#scale, proposal A): the signals that surface and badge REGARDLESS
/// of a source's Quiet / Digest / Muted level — the Gmail/Slack
/// contract that makes muting feel safe. Deliberately narrower than
/// [`workspace_attention_signals`]: ambient unread and
/// somebody-requested-a-review-from-someone don't qualify; only things
/// addressed at *you* or owned by you do.
///
/// - an agent in the workspace is asking for input,
/// - a review is requested of YOU (viewer role is Reviewer with the
///   review still pending / returned),
/// - YOUR own PR's CI is failing,
/// - you are @mentioned and the row has unread activity.
pub fn punches_through(
    w: &Workspace,
    agents: &HashMap<SessionKey, lazybox_ipc::AgentState>,
) -> bool {
    if crate::agent_attention::workspace_is_asking(w, agents) {
        return true;
    }
    let Some(t) = w.primary_task() else {
        return false;
    };
    match t.role {
        lazybox_core::TaskRole::Reviewer => matches!(
            t.review,
            lazybox_core::ReviewStatus::Pending | lazybox_core::ReviewStatus::ChangesRequested,
        ),
        lazybox_core::TaskRole::Author => matches!(
            t.ci,
            lazybox_core::CiStatus::Failure | lazybox_core::CiStatus::Mixed
        ),
        lazybox_core::TaskRole::Mentioned => w.unread_count() > 0,
        lazybox_core::TaskRole::Assignee => false,
    }
}
