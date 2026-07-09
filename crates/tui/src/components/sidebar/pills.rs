//! Rendering primitives + visibility/attention helpers for the
//! sidebar. Everything that turns workspace state into visual
//! signal — status pills, role badges, mailbox membership,
//! attention scoring, column widths — lives here so `mod.rs` can
//! stay focused on the `Sidebar` struct + its impl blocks.
//!
//! Why one file: these helpers share the same conceptual layer
//! ("how does a workspace surface in the inbox?") and they form a
//! tightly-knit graph — `status_pills` calls into `lifecycle_pill`;
//! `mailbox_membership` is consulted by render; `relative_time`
//! and `role_badge` are render primitives too. Splitting them
//! across separate files would mean a lot of `pub(super) use`
//! gymnastics for no real readability win.

use super::Mailbox;
use lazybox_core::{SessionKey, Workspace};
use ratatui::style::{Color, Modifier, Style};

/// Right-side status pill showing the most actionable problem on the
/// PR. One pill at a time, ordered by severity: merge conflict beats
/// CI failure beats CI mixed beats CI running beats CI ok beats
/// "behind base" beats nothing. The pill is a colored block
/// (` CONFLICT ` / ` CI FAIL ` / ` CI OK ` / etc.) with strong fg +
/// colored bg — the v1 design that the user actually likes; subtle
/// text-only failed visually.
pub(crate) struct StatusPill {
    pub(crate) label: &'static str,
    pub(crate) style: Style,
}

/// Does this workspace want the user's attention right now? Drives
/// the "needs attention" counter on the collapsed repo header.
/// Each signal (unread / CI / review / agent-asking / mentioned)
/// is independently toggleable via `~/.lazybox/config.yaml::attention`.
/// Single-cell type marker rendered immediately before the number
/// on each workspace row. The glyph sits flush against the number
/// (`⇄312`, not `[PR]   #312`) so the title column gets the cells
/// back — see issue #42. The `#` prefix was dropped once the glyph
/// alone carried the issue-vs-PR signal — see issue #67.
///
/// Variants (default unicode):
/// - `⇄` — workspaces holding a pull request.
/// - `○` — github-issue-only workspaces.
/// - `◆` — linear-ticket-only workspaces. Distinct source (different
///   keymaps later, no review threads, no PRs, …).
/// - `None` — empty scratch workspaces (no PR, no issues).
///
/// `ascii` toggles the fallback letters (`p` / `i` / `l`) for fonts
/// that don't render the unicode glyphs reliably as a single cell.
/// Wired from `display.ascii_glyphs` in `~/.lazybox/config.yaml`.
pub(crate) fn workspace_type_label(workspace: &Workspace, ascii: bool) -> Option<&'static str> {
    if workspace.pr.is_some() {
        return Some(if ascii { "p" } else { "⇄" });
    }
    if !workspace.gh_issues.is_empty() {
        return Some(if ascii { "i" } else { "○" });
    }
    if !workspace.linear_issues.is_empty() {
        return Some(if ascii { "l" } else { "◆" });
    }
    None
}

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

pub fn mailbox_membership(
    workspace: &Workspace,
    mailbox: Mailbox,
    now: chrono::DateTime<chrono::Utc>,
    show_inactive_in_inbox: bool,
) -> bool {
    let snoozed = workspace.is_snoozed(now);
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
    agents_asking: &std::collections::HashSet<SessionKey>,
) -> Vec<AttentionSignal> {
    let mut out = Vec::new();
    if w.unread_count() > 0 {
        out.push(AttentionSignal::Unread);
    }
    // AgentAsking signal: source of truth is the sidebar-local
    // `agents_asking` set (driven by `Event::AgentState` deltas).
    // NOT `w.sessions[i].state` — that gets blown away every poll
    // when `WorkspaceUpserted` re-loads from the persisted store.
    if crate::agent_attention::workspace_is_asking(w, agents_asking) {
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
pub(crate) fn attention_gate(
    signal: AttentionSignal,
    cfg: &lazybox_config::AttentionConfig,
) -> bool {
    match signal {
        AttentionSignal::Unread => cfg.unread,
        AttentionSignal::AgentAsking => cfg.agent_asking,
        AttentionSignal::CiFailing => cfg.ci_failing,
        AttentionSignal::ReviewPending => cfg.review_pending,
        AttentionSignal::Mentioned => cfg.mentioned,
    }
}

pub(crate) fn workspace_needs_attention(
    w: &Workspace,
    cfg: &lazybox_config::AttentionConfig,
    agents_asking: &std::collections::HashSet<SessionKey>,
) -> bool {
    workspace_attention_signals(w, agents_asking)
        .iter()
        .any(|s| attention_gate(*s, cfg))
}

/// Render the right-trailer pill for a task. **Pure mapping** from
/// `StatusTag::for_task(task)` — no priority logic lives here, all
/// of that is in `lazybox_core::task::StatusTag::for_task`. Adding a
/// new visual state means adding a `StatusTag` variant first; the
/// match below is exhaustive, so the compiler then catches the
/// missing pill arm.
///
/// Returning `Option<StatusPill>` so the `StatusTag::None` case
/// renders as a hole (no pill) without making every caller filter.
#[cfg(test)]
pub(crate) fn status_pill(task: &lazybox_core::Task) -> Option<StatusPill> {
    pill_for_tag(lazybox_core::StatusTag::for_task(task))
}

/// Two-column status: a (review-or-lifecycle, ci) pair. Review and
/// CI are conceptually orthogonal — squashing them into one pill
/// hides one of the two signals. A PR with `REVIEW pending + CI
/// FAIL` is something the user needs to see BOTH of.
///
/// **Terminal / blocker** states (`MERGED`, `CLOSED`, `DRAFT`,
/// `CONFLICT`, `READY`, `QUEUED`, `AUTO`) are single-purpose — they
/// override both signals — so they return as the first slot with
/// the CI slot empty.
///
/// **Open PRs in flight** return `(review_pill, ci_pill)` so the
/// row shows both. Either slot may be `None` (e.g. CI not yet
/// configured → ci=None; new PR with no review activity → review=None).
pub(crate) fn status_pills(task: &lazybox_core::Task) -> (Option<StatusPill>, Option<StatusPill>) {
    use lazybox_core::{CiStatus, ReviewStatus, TaskState};
    if let Some(lifecycle) = lifecycle_pill(task) {
        return (Some(lifecycle), None);
    }
    // Open PR in flight. Review + CI rendered separately.
    let review = match task.review {
        ReviewStatus::Approved => Some(StatusPill {
            label: " APPROVED",
            style: Style::default()
                .bg(crate::theme::current().accent)
                .fg(Color::Black)
                .add_modifier(Modifier::BOLD),
        }),
        ReviewStatus::ChangesRequested => Some(StatusPill {
            label: " CHANGES ",
            style: Style::default()
                .bg(Color::Indexed(196))
                .fg(Color::Black)
                .add_modifier(Modifier::BOLD),
        }),
        ReviewStatus::Pending => Some(StatusPill {
            label: " REVIEW  ",
            style: Style::default()
                .bg(crate::theme::current().warn)
                .fg(Color::Black)
                .add_modifier(Modifier::BOLD),
        }),
        ReviewStatus::None => None,
    };
    let ci = match task.ci {
        CiStatus::Failure => Some(StatusPill {
            label: " CI FAIL ",
            style: Style::default()
                .bg(Color::Indexed(196))
                .fg(Color::Black)
                .add_modifier(Modifier::BOLD),
        }),
        CiStatus::Mixed => Some(StatusPill {
            label: " CI MIX  ",
            style: Style::default()
                .bg(Color::Indexed(214))
                .fg(Color::Black)
                .add_modifier(Modifier::BOLD),
        }),
        CiStatus::Pending | CiStatus::Running => Some(StatusPill {
            label: " CI RUN  ",
            style: Style::default()
                .bg(Color::Indexed(220))
                .fg(Color::Black)
                .add_modifier(Modifier::BOLD),
        }),
        CiStatus::Success => Some(StatusPill {
            label: " CI OK   ",
            style: Style::default()
                .bg(Color::Indexed(40))
                .fg(Color::Black)
                .add_modifier(Modifier::BOLD),
        }),
        CiStatus::None => None,
    };
    // Open issue (no PR) or no signals at all → keep both columns
    // empty rather than showing stale review/ci state from a
    // non-existent PR. Caller still reserves the column space so
    // the time trailer doesn't shift.
    if task.state != TaskState::Open && task.state != TaskState::InReview {
        return (None, None);
    }
    (review, ci)
}

/// Single-pill lifecycle override: returns `Some` for states that
/// take over the whole status area (no review+ci pair beside them).
/// `None` for normal open PRs in flight.
fn lifecycle_pill(task: &lazybox_core::Task) -> Option<StatusPill> {
    use lazybox_core::{CiStatus, ReviewStatus, StatusTag, TaskState};
    let pill_red = Style::default()
        .bg(Color::Indexed(196))
        .fg(Color::Black)
        .add_modifier(Modifier::BOLD);
    let pill_green = Style::default()
        .bg(Color::Indexed(40))
        .fg(Color::Black)
        .add_modifier(Modifier::BOLD);
    let theme = crate::theme::current();
    // Match the priority chain in StatusTag::for_task for the
    // terminal / blocker bands. The non-blocker tags
    // (CiFailed/CiMixed/CiRunning/CiOk + Approved/Changes/Review)
    // are handled by the side-by-side pills in `status_pills`.
    match task.state {
        TaskState::Merged => {
            return Some(StatusPill {
                label: " MERGED  ",
                style: Style::default()
                    .bg(theme.hover)
                    .fg(Color::Black)
                    .add_modifier(Modifier::BOLD),
            });
        }
        TaskState::Closed => {
            return Some(StatusPill {
                label: " CLOSED  ",
                style: Style::default()
                    .bg(theme.error)
                    .fg(Color::Black)
                    .add_modifier(Modifier::BOLD),
            });
        }
        TaskState::Draft => {
            return Some(StatusPill {
                label: " DRAFT   ",
                style: Style::default()
                    .bg(theme.chrome)
                    .fg(theme.text_strong)
                    .add_modifier(Modifier::BOLD),
            });
        }
        _ => {}
    }
    if task.mergeable.is_conflicting() {
        return Some(StatusPill {
            label: " CONFLICT",
            style: pill_red,
        });
    }
    if task.is_in_merge_queue {
        return Some(StatusPill {
            label: " QUEUED  ",
            style: pill_green,
        });
    }
    // Approved + CI green = READY (one pill, end-state for "this PR
    // is good to go"). Approved-without-green-CI shows up as a
    // separate APPROVED pill in the review slot via `status_pills`.
    if task.review == ReviewStatus::Approved
        && matches!(task.ci, CiStatus::Success | CiStatus::None)
    {
        return Some(StatusPill {
            label: " READY   ",
            style: pill_green,
        });
    }
    if task.auto_merge_enabled {
        return Some(StatusPill {
            label: " AUTO    ",
            style: Style::default()
                .bg(theme.accent)
                .fg(Color::Black)
                .add_modifier(Modifier::BOLD),
        });
    }
    // Behind base is informational only — don't fight for the
    // lifecycle column. `StatusTag` keeps surfacing it via the old
    // path for any consumer that still wants it.
    let _ = StatusTag::for_task(task);
    None
}

/// Pure tag → pill mapping. Exists as its own function so the
/// contract tests (`status_pill_consistency_tests`) can pin every
/// `(StatusTag, StatusPill)` pair without going through a
/// constructed `Task`.
#[cfg(test)]
pub(crate) fn pill_for_tag(tag: lazybox_core::StatusTag) -> Option<StatusPill> {
    use lazybox_core::StatusTag::*;
    let theme = crate::theme::current();
    // Indexed palette colors render as the terminal's "bright"
    // red/yellow on most setups — punchy without the muddy mid-red
    // `Color::Red` produces on dark themes. Black-on-color reads
    // cleaner than white-on-color at this size.
    let pill_red = Style::default()
        .bg(Color::Indexed(196))
        .fg(Color::Black)
        .add_modifier(Modifier::BOLD);
    let pill_amber = Style::default()
        .bg(Color::Indexed(214))
        .fg(Color::Black)
        .add_modifier(Modifier::BOLD);
    let pill_yellow = Style::default()
        .bg(Color::Indexed(220))
        .fg(Color::Black)
        .add_modifier(Modifier::BOLD);
    let pill_green = Style::default()
        .bg(Color::Indexed(40))
        .fg(Color::Black)
        .add_modifier(Modifier::BOLD);
    let pill = |label: &'static str, style: Style| Some(StatusPill { label, style });
    match tag {
        Merged => pill(
            " MERGED   ",
            Style::default()
                .bg(theme.hover)
                .fg(Color::Black)
                .add_modifier(Modifier::BOLD),
        ),
        Closed => pill(
            " CLOSED   ",
            Style::default()
                .bg(theme.error)
                .fg(Color::Black)
                .add_modifier(Modifier::BOLD),
        ),
        Conflict => pill(" CONFLICT ", pill_red),
        CiFailed => pill(" CI FAIL  ", pill_red),
        CiMixed => pill(" CI MIX   ", pill_amber),
        ChangesRequested => pill(" CHANGES  ", pill_red),
        Queued => pill(" QUEUED   ", pill_green),
        Draft => pill(
            " DRAFT    ",
            Style::default()
                .bg(theme.chrome)
                .fg(theme.text_strong)
                .add_modifier(Modifier::BOLD),
        ),
        Ready => pill(" READY    ", pill_green),
        Approved => pill(
            " APPROVED ",
            Style::default()
                .bg(theme.accent)
                .fg(Color::Black)
                .add_modifier(Modifier::BOLD),
        ),
        AutoMerge => pill(
            " AUTO     ",
            Style::default()
                .bg(theme.accent)
                .fg(Color::Black)
                .add_modifier(Modifier::BOLD),
        ),
        ReviewPending => pill(
            " REVIEW   ",
            Style::default()
                .bg(theme.warn)
                .fg(Color::Black)
                .add_modifier(Modifier::BOLD),
        ),
        CiRunning => pill(" CI RUN   ", pill_yellow),
        CiOk => pill(" CI OK    ", pill_green),
        Behind => pill(
            " BEHIND   ",
            Style::default()
                .fg(theme.text_dim)
                .add_modifier(Modifier::BOLD),
        ),
        None => Option::None,
    }
}

/// Compact relative time for the right-side trailer. `now` < 1m → "now",
/// < 1h → `Xm`, < 24h → `Xh`, < 30d → `Xd`, else `Xmo`. Always 2-3
/// cells so the column lines up.
pub(crate) fn relative_time(
    then: chrono::DateTime<chrono::Utc>,
    now: chrono::DateTime<chrono::Utc>,
) -> String {
    let delta = now.signed_duration_since(then);
    let secs = delta.num_seconds().max(0);
    if secs < 60 {
        return "now".into();
    }
    let mins = secs / 60;
    if mins < 60 {
        return format!("{mins}m");
    }
    let hours = mins / 60;
    if hours < 24 {
        return format!("{hours}h");
    }
    let days = hours / 24;
    if days < lazybox_core::time::DAYS_PER_MONTH {
        return format!("{days}d");
    }
    let months = days / lazybox_core::time::DAYS_PER_MONTH;
    format!("{months}mo")
}

/// Single-letter role marker + color for the workspace row. Renders
/// as a leading badge (`A` for author, `R` for reviewer, `@` for
/// assignee, dim `·` for mentioned/done) so the user can scan the
/// inbox and pick out "PRs I have to act on" without reading titles.
///
/// All colors come from the active theme — no hardcoded RGB — so
/// the badges sit on the same palette as the rest of the UI and
/// don't fight for attention.
pub(crate) fn role_badge(
    theme: &crate::theme::Theme,
    role: lazybox_core::TaskRole,
) -> (char, Color) {
    match role {
        lazybox_core::TaskRole::Author => ('A', theme.success),
        lazybox_core::TaskRole::Reviewer => ('R', theme.accent),
        lazybox_core::TaskRole::Assignee => ('@', theme.warn),
        lazybox_core::TaskRole::Mentioned => ('·', theme.text_dim),
    }
}

/// Per-letter pill style for the runner badge. Subtle bg tint (chrome
/// grey, not a saturated accent) + colored bold fg so the pills read
/// clearly without competing with status colors elsewhere on the row.
pub(crate) fn badge_pill_style(theme: &crate::theme::Theme, letter: char) -> Style {
    let fg = match letter {
        'C' => theme.accent,
        'X' => theme.warn,
        'U' => theme.success,
        'S' => theme.text_strong,
        _ => theme.text_strong,
    };
    Style::default()
        .bg(theme.fill)
        .fg(fg)
        .add_modifier(Modifier::BOLD)
}

/// Visual width in cells of a string, treating each char as one cell.
/// PR titles in the sidebar are ASCII in practice; the prefixes use
/// `▸` (1-cell box-drawing) which `chars().count()` gets right.
pub(super) fn visual_width(s: &str) -> usize {
    s.chars().count()
}

/// Truncate `s` so it fits in `budget` cells, adding `…` when clipped.
/// Returns `s` unchanged when it already fits (cheap fast-path).
pub(crate) fn truncate_ellipsis(s: &str, budget: usize) -> String {
    let w = visual_width(s);
    if w <= budget {
        return s.to_string();
    }
    if budget == 0 {
        return String::new();
    }
    if budget == 1 {
        return "…".to_string();
    }
    // Take `budget - 1` chars, append the ellipsis. Iterating chars
    // (not bytes) keeps multi-byte UTF-8 intact.
    let mut out: String = s.chars().take(budget - 1).collect();
    out.push('…');
    out
}
