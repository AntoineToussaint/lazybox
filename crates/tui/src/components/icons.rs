//! Nerd Font glyphs for the TUI.
//!
//! These are private-use-area codepoints that render as graphical
//! icons when the user's terminal font has Nerd Font glyph patches.
//! On a non-Nerd-Font terminal they render as `?` or empty boxes —
//! we accept that trade-off because the alternative (a runtime probe
//! and ASCII-fallback table) doubles the maintenance cost for a
//! tiny minority of users.
//!
//! Naming follows the call-site, not the icon (`comment`, not
//! `speech_bubble`), so future icon swaps don't churn every consumer.

/// Repo / source tree row in the sidebar.
pub const REPO: &str = "";

// ── Activity kinds ────────────────────────────────────────────────────

pub const COMMENT: &str = "";
pub const REVIEW: &str = "";
pub const STATUS_CHANGE: &str = "";
pub const CI: &str = "";

// ── PR roles (sidebar leading badge) ──────────────────────────────────

/// Author — `nf-fa-pencil`. "I wrote this PR."
pub const ROLE_AUTHOR: &str = "";
/// Reviewer — `nf-fa-eye`. "Someone wants me to look at this."
pub const ROLE_REVIEWER: &str = "";
/// Assignee — `nf-fa-user`. "Assigned to me to ship."
pub const ROLE_ASSIGNEE: &str = "";
/// Mentioned / FYI — `nf-fa-bell`. Lower priority — heard but not tasked.
pub const ROLE_MENTIONED: &str = "";

// ── PR status indicators (sidebar trailing slot) ──────────────────────

/// Merge conflict — `nf-fa-exclamation_triangle`. Hard blocker.
pub const STATUS_CONFLICT: &str = "";
/// CI failed — `nf-fa-times_circle`. Tests or build red.
pub const STATUS_CI_FAIL: &str = "";
/// CI mixed — `nf-fa-question_circle`. Some checks red, some green.
pub const STATUS_CI_MIX: &str = "";
/// CI pending / running — `nf-fa-clock_o`. Waiting on the build.
pub const STATUS_CI_PENDING: &str = "";
/// Branch is behind base — `nf-fa-arrow_down`. Needs an update.
pub const STATUS_BEHIND: &str = "";

// ── Runner kinds ──────────────────────────────────────────────────────

/// Generic robot — used for the Claude / Codex agent runners (the user
/// recognises the icon-as-bot pattern from terminal apps generally;
/// brand-specific glyphs aren't in the Nerd Font set).
pub const AGENT: &str = "";
/// Cursor = a pointer; ties the agent's name to its icon.
pub const CURSOR: &str = "";
/// Shell prompt.
pub const SHELL: &str = "";

// ── PR / task states ──────────────────────────────────────────────────

pub const PR_OPEN: &str = "";
pub const PR_DRAFT: &str = "";
pub const PR_MERGED: &str = "";
pub const PR_CLOSED: &str = "";
pub const PR_REVIEW: &str = "";
pub const PR_WIP: &str = "";

/// Nerd-Font triangle that "closes" a powerline segment by extending
/// the segment's color into one cell of empty space. Used in the
/// status footer + PR state pill.
pub const POWERLINE_RIGHT: &str = "";

/// "Opens in browser" affordance on clickable links. The trailing
/// `U+FE0E` (variation selector-15) forces *text* presentation, so
/// terminals that would otherwise render U+2197 as a 2-cell emoji keep
/// it to the single cell `unicode-width` budgets for it — without this
/// the glyph can overhang the layout on emoji-presentation terminals.
pub const EXTERNAL_LINK: &str = "↗\u{FE0E}";

/// Pick the right runner-kind icon for an agent id. Falls back to the
/// generic `AGENT` glyph for unknown ids — e.g. user-defined agents
/// from `~/.lazybox/config.yaml`.
pub fn agent_icon(agent_id: &str) -> &'static str {
    match agent_id {
        "cursor" => CURSOR,
        // Claude / Codex / generic CLIs all reuse the bot glyph today.
        // Easy to specialise later if Nerd Font adds dedicated icons.
        _ => AGENT,
    }
}
