//! Ratatui-styled rendering primitives for the sidebar: status pills,
//! role badges, the workspace-type glyph, relative-time trailer, and
//! badge styling. The client-free visibility/attention producers
//! (`mailbox_membership`, `workspace_attention_signals`, …) live in
//! `lazybox_tui_core::inbox`; this file only turns their output into
//! ratatui `Style`s so `mod.rs` can stay focused on the `Sidebar`
//! struct + its impl blocks.
//!
//! Why one file: these helpers share the same conceptual layer
//! ("how does a workspace surface in the inbox?") and they form a
//! tightly-knit graph — `status_pills` calls into `lifecycle_pill`;
//! `relative_time` and `role_badge` are render primitives too.
//! Splitting them across separate files would mean a lot of
//! `pub(super) use` gymnastics for no real readability win.

use lazybox_core::Workspace;
use ratatui::style::{Color, Modifier, Style};

/// Right-side status glyph showing the most actionable signal on the
/// PR. Two slots at most — one review glyph, one CI glyph — unless a
/// terminal/blocker lifecycle state (merged, closed, conflict, …)
/// overrides both. Each glyph is a single fg-colored cell (`✓` / `✗` /
/// `±` / `⚠` / …) rather than the wide text pills the row used to carry
/// (` CI FAIL `, ` APPROVED `): color is the primary signal, the glyph
/// the secondary, and the reclaimed cells go to the title (#1046). The
/// full meaning stays discoverable in the `?` help legend
/// ([`status_legend`]) and the Ask Lazybox marker docs.
pub(crate) struct StatusPill {
    /// The rendered glyph, with a leading space so two slots separate
    /// visually (` ✓ ✗`). Trimmed, this is what the marker docs pin to.
    pub(crate) label: &'static str,
    pub(crate) style: Style,
}

// ── Status glyphs ─────────────────────────────────────────────────────
//
// Single-cell BMP symbols (not Nerd-Font PUA — those render as tofu
// without a patched font), colored by severity so the glyph reads even
// where two adjacent slots share one (approved `✓` beside CI-ok `✓`).
// Kept in sync with `lazybox_tui_core::markers` by the
// `documented_status_pills_match_the_renderer` drift test.
// Each carries a leading-space separator so two adjacent slots read as
// ` ✓ ✗`; trimmed, the glyph is what the marker docs pin to.
const G_OK: &str = " ✓"; // CI green / approved / ready / merged
const G_FAIL: &str = " ✗"; // CI failed / changes requested
const G_MIXED: &str = " ±"; // CI partly green, partly failing
const G_RUNNING: &str = " ◔"; // CI queued or in progress — a partial-circle,
// deliberately NOT `…`, which collides with the title's truncation ellipsis.
const G_REVIEW: &str = " ◌"; // review requested / pending
// U+FE0E (text-presentation selector) pins `⚠` to one cell: bare U+26A0 is
// width-ambiguous and many emoji-capable terminals render it two cells wide
// while `unicode-width` measures one, drifting the trailer by a cell on
// conflict rows. The selector forces the narrow, measured form.
const G_CONFLICT: &str = " ⚠\u{fe0e}"; // merge conflict
const G_CLOSED: &str = " ⊘"; // closed without merging
const G_DRAFT: &str = " ◇"; // draft PR
const G_QUEUED: &str = " ⧖"; // sitting in the merge queue
#[cfg(test)]
const G_BEHIND: &str = " ↓"; // branch behind its base (tag-map only; no row pill)

/// A fg-colored, bold status glyph with a leading-space separator.
///
/// `fg` MUST be a theme-derived tone (`theme.success`/`error`/`warn`/…),
/// not a fixed `Color::Indexed(..)`: these are foreground glyphs on
/// `theme.surface`, so — unlike the old black-on-color pill *fills*, which
/// held contrast on any background — a bright fixed palette color (green
/// `40`, amber `214`, yellow `220`) is unreadable on the near-white
/// Lazybox Light surface. The theme tones are calibrated for contrast on
/// both the light and dark backgrounds (#1046).
fn glyph_pill(label: &'static str, fg: Color) -> StatusPill {
    StatusPill {
        label,
        style: Style::default().fg(fg).add_modifier(Modifier::BOLD),
    }
}

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

/// Two-column status: a (review-or-lifecycle, ci) pair of glyphs. Review
/// and CI are conceptually orthogonal — squashing them into one signal
/// hides one of the two. A PR with review-pending + CI-failing (`◌ ✗`)
/// is something the user needs to see BOTH of.
///
/// **Terminal / blocker** states (merged `✓`, closed `⊘`, draft `◇`,
/// conflict `⚠`, ready `✓`, queued `⧖`) are single-purpose — they
/// override both signals — so they return as the first slot with the CI
/// slot empty. GitHub-native auto-merge is *not* one of them: it's a
/// policy, rendered as its own `◆` row glyph (#778), so a red-CI armed PR
/// still shows its `✗` here.
///
/// **Open PRs in flight** return `(review_pill, ci_pill)` so the
/// row shows both. Either slot may be `None` (e.g. CI not yet
/// configured → ci=None; new PR with no review activity → review=None).
pub(crate) fn status_pills(task: &lazybox_core::Task) -> (Option<StatusPill>, Option<StatusPill>) {
    use lazybox_core::{CiStatus, ReviewStatus, TaskState};
    if let Some(lifecycle) = lifecycle_pill(task) {
        return (Some(lifecycle), None);
    }
    // Open PR in flight. Review + CI rendered separately, as adjacent
    // single-cell glyphs (` ◌ ✗` = review pending + CI failing). Colors are
    // theme-derived so they hold contrast on the light theme too (#1046).
    let theme = crate::theme::current();
    let review = match task.review {
        ReviewStatus::Approved => Some(glyph_pill(G_OK, theme.accent)),
        ReviewStatus::ChangesRequested => Some(glyph_pill(G_FAIL, theme.error)),
        ReviewStatus::Pending => Some(glyph_pill(G_REVIEW, theme.warn)),
        ReviewStatus::None => None,
    };
    let ci = match task.ci {
        CiStatus::Failure => Some(glyph_pill(G_FAIL, theme.error)),
        CiStatus::Mixed => Some(glyph_pill(G_MIXED, theme.warn)),
        CiStatus::Pending | CiStatus::Running => Some(glyph_pill(G_RUNNING, theme.accent)),
        CiStatus::Success => Some(glyph_pill(G_OK, theme.success)),
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
    use lazybox_core::{CiStatus, ReviewStatus, TaskState};
    let theme = crate::theme::current();
    // Match the priority chain in StatusTag::for_task for the
    // terminal / blocker bands. The non-blocker tags
    // (CiFailed/CiMixed/CiRunning/CiOk + Approved/Changes/Review)
    // are handled by the side-by-side pills in `status_pills`.
    match task.state {
        TaskState::Merged => return Some(glyph_pill(G_OK, theme.hover)),
        TaskState::Closed => return Some(glyph_pill(G_CLOSED, theme.text_dim)),
        // `theme.chrome` is a near-surface grey (the old draft pill used it
        // as a *fill* behind `text_strong`); as a foreground glyph it's
        // invisible on the light surface, so use the contrast-tuned
        // `text_dim` — draft reads as "inactive," which fits (#1046).
        TaskState::Draft => return Some(glyph_pill(G_DRAFT, theme.text_dim)),
        _ => {}
    }
    if task.mergeable.is_conflicting() {
        return Some(glyph_pill(G_CONFLICT, theme.error));
    }
    if task.is_in_merge_queue {
        return Some(glyph_pill(G_QUEUED, theme.success));
    }
    // Approved + CI green = READY (one pill, end-state for "this PR
    // is good to go"). Approved-without-green-CI shows up as a
    // separate APPROVED pill in the review slot via `status_pills`.
    // A ruleset/branch-protection block (`merge_blocked`) withholds
    // READY even when approved + green — GitHub would bounce the merge,
    // so it isn't "good to go" (issue #998).
    if task.review == ReviewStatus::Approved
        && matches!(task.ci, CiStatus::Success | CiStatus::None)
        && !task.merge_blocked
    {
        return Some(glyph_pill(G_OK, theme.success));
    }
    // GitHub-native auto-merge (`task.auto_merge_enabled`) is a standing
    // automation policy, not a task status — it renders as its own row
    // glyph (`◆`, see `workspace_row::cell_auto`) alongside `⚡` / `🔧`,
    // never here. Placing it in this chain hid the `✗` CI-fail glyph on
    // exactly the armed PRs that need it most (#778).
    //
    // Everything else — behind-base and plain open PRs — has no lifecycle
    // override; the review + CI pair in `status_pills` carries the signal.
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
    match tag {
        Merged => Some(glyph_pill(G_OK, theme.hover)),
        Closed => Some(glyph_pill(G_CLOSED, theme.text_dim)),
        Conflict => Some(glyph_pill(G_CONFLICT, theme.error)),
        CiFailed => Some(glyph_pill(G_FAIL, theme.error)),
        CiMixed => Some(glyph_pill(G_MIXED, theme.warn)),
        ChangesRequested => Some(glyph_pill(G_FAIL, theme.error)),
        Queued => Some(glyph_pill(G_QUEUED, theme.success)),
        Draft => Some(glyph_pill(G_DRAFT, theme.text_dim)),
        Ready => Some(glyph_pill(G_OK, theme.success)),
        Approved => Some(glyph_pill(G_OK, theme.accent)),
        ReviewPending => Some(glyph_pill(G_REVIEW, theme.warn)),
        CiRunning => Some(glyph_pill(G_RUNNING, theme.accent)),
        CiOk => Some(glyph_pill(G_OK, theme.success)),
        Behind => Some(glyph_pill(G_BEHIND, theme.text_dim)),
        None => Option::None,
    }
}

// ── Policy-badge glyphs ───────────────────────────────────────────────
//
// The merge-on-green arms and auto-fix badge, rendered by
// `workspace_row` and described in the `?` legend. `◆` (auto-merge) and
// `⤓` (track-main) are already trusted single-cell glyphs elsewhere in
// the sidebar.
pub(crate) const ARM_GLYPH: &str = "⚡"; // lazybox client-side merge-on-green
pub(crate) const AUTO_GLYPH: &str = "◆"; // GitHub-native auto-merge
pub(crate) const FIX_GLYPH: &str = "🔧"; // auto-fix armed
pub(crate) const TRACK_GLYPH: &str = "⤓"; // track-main (auto-sync to default branch)

/// One row of the sidebar status-icon legend shown in the `?` help
/// modal: the glyph in its real theme color plus a one-line meaning.
/// Built here (client side) so the colors come straight from the active
/// theme — the tui-core marker docs carry the same meanings as plain
/// text for Ask Lazybox, but only the UI can paint the swatch.
pub(crate) struct LegendRow {
    pub(crate) glyph: &'static str,
    pub(crate) style: Style,
    pub(crate) meaning: &'static str,
}

/// The full sidebar status-icon legend, grouped CI → review → lifecycle
/// → policy. Every glyph a workspace row's status/policy columns can
/// carry, so the compact icons stay discoverable (#1046).
pub(crate) fn status_legend() -> Vec<LegendRow> {
    let theme = crate::theme::current();
    let row = |glyph: &'static str, fg: Color, meaning: &'static str| LegendRow {
        glyph,
        style: Style::default().fg(fg).add_modifier(Modifier::BOLD),
        meaning,
    };
    vec![
        row(G_OK.trim(), theme.success, "CI passing"),
        row(G_FAIL.trim(), theme.error, "CI failing / changes requested"),
        row(G_MIXED.trim(), theme.warn, "CI partly failing"),
        row(G_RUNNING.trim(), theme.accent, "CI running"),
        row(G_OK.trim(), theme.accent, "approved"),
        row(G_REVIEW.trim(), theme.warn, "review requested / pending"),
        row(G_CONFLICT.trim(), theme.error, "merge conflict"),
        row(G_QUEUED.trim(), theme.success, "in the merge queue"),
        row(G_DRAFT.trim(), theme.text_dim, "draft"),
        row(G_OK.trim(), theme.hover, "merged"),
        row(G_CLOSED.trim(), theme.text_dim, "closed"),
        row(
            ARM_GLYPH,
            theme.success,
            "merge-on-green armed (lazybox, g g)",
        ),
        row(AUTO_GLYPH, theme.accent, "GitHub auto-merge enabled"),
        row(FIX_GLYPH, theme.warn, "auto-fix armed"),
        row(
            TRACK_GLYPH,
            theme.accent,
            "track-main (auto-sync to default)",
        ),
    ]
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
