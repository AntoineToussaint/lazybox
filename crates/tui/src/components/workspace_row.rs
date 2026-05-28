//! Declarative workspace-row layout for the sidebar.
//!
//! Replaces ~200 LoC of hand-rolled span-stitching + width tracking
//! that used to live inline in `Sidebar::render`. Defines the
//! workspace row as a sequence of typed columns + per-piece cell
//! builders; the table primitive (`components::table`) handles
//! geometry (column widths, padding, right-alignment, cursor fill).
//!
//! Each cell builder is a pure function of `&WorkspaceRowCtx` so
//! callers can unit-test individual pieces (the PR-number cell's
//! padding behavior, the status pill's row-style fallback, the
//! asking glyph's reserved width) without rendering a whole
//! sidebar.

use crate::components::sidebar::{
    StatusPill, badge_pill_style, role_badge, status_pills, workspace_type_label,
};
use crate::components::table::{Cell, Column, Row};
use crate::theme::Theme;
use pilot_core::{Task, Workspace};
use ratatui::style::{Modifier, Style};
use ratatui::text::Span;

/// All state needed to render one workspace row. Built once per
/// row by the sidebar's render fn from `self` + `(visible_row, i)`.
/// Borrowed everywhere so we don't allocate for the typical case.
pub struct WorkspaceRowCtx<'a> {
    pub workspace: Option<&'a Workspace>,
    pub task: Option<&'a Task>,
    pub theme: &'a Theme,
    pub now: chrono::DateTime<chrono::Utc>,
    pub focused: bool,
    pub is_cursor: bool,
    /// Widest `#NNN` across all visible workspace rows in this
    /// render pass. Every row's pr-number cell pads to this width
    /// so the role / asking columns line up across rows.
    pub max_pr_num_width: usize,
    /// `LatchSet::armed(...) == Some(this_key)` for the long-snooze
    /// latch — paints the `[snooze 1y?]` chrome. The kill latch
    /// retired when Archive moved to a Confirm modal (every
    /// destructive action goes through `ActionConfirm` now).
    pub long_snooze_armed: bool,
    /// Any agent in this workspace is in `AgentState::Asking`.
    pub asking: bool,
    /// `Sidebar::runner_badges(key)` — `[('C', n), ('S', m)]` etc.
    pub badges: Vec<(char, usize)>,
    /// Render the type indicator as plain ASCII (`p`/`i`/`l`) instead
    /// of the default unicode glyphs (`⇄`/`○`/`◆`). Wired from
    /// `display.ascii_glyphs` in `~/.pilot/config.yaml`.
    pub ascii_glyphs: bool,
}

impl<'a> WorkspaceRowCtx<'a> {
    /// Cursor row background. Drives `Row::fill_style` so every
    /// column's padding inherits the highlight bg — without this
    /// the cursor row looked broken (highlight stopping mid-row).
    pub fn row_style(&self) -> Style {
        if self.is_cursor && self.focused {
            self.theme.row_focused()
        } else if self.is_cursor {
            self.theme.row_unfocused()
        } else {
            Style::default()
        }
    }

    fn raw_title(&self) -> &'a str {
        self.task
            .map(|t| t.title.as_str())
            .unwrap_or_else(|| self.workspace.map(|w| w.name.as_str()).unwrap_or("?"))
    }
}

/// Column spec for every workspace row in the current render pass.
/// Built once (with `max_pr_num_width` from the pre-pass), and shared by
/// a SINGLE `render_table` call that takes every visible workspace row
/// at once — that's what lets `Column::max(0)` line up across rows
/// (each Max column expands to the widest natural cell across the whole
/// table, and collapses to 0 when no row has content).
///
/// Order (left → right):
///
/// 0. Prefix — `  ▸ ` (cursor) / `    ` (no cursor).
/// 1. Type glyph — `⇄` / `○` / `◆` (or ASCII `p`/`i`/`l`) / blank.
///    Exactly 1 cell so it sits flush against the `#NNN` to its right
///    — see issue #42.
/// 2. PR number — `#NNN`, padded to `max_pr_num_width`.
/// 3. Role badge — ` R` colored marker, or blank.
/// 4. Asking glyph — ` ? ` warn-colored, or blank — reserved width so
///    the kind/title to the right don't jitter between asking /
///    not-asking rows.
/// 5. Kind label — `[FEAT] ` etc, or blank. Max across rows so titles
///    align even when some rows have no kind prefix.
/// 6. Title — flex, absorbs the remaining width. Truncates with `…`.
/// 7. Kill mark — ` [snooze 1y?]`, or blank. Max so the title flex
///    reclaims the space when no row is armed.
/// 8. Unread pill — ` ●N `, right-aligned. Max so the column collapses
///    when no row has unread, and lines up at a consistent x when any
///    row does.
/// 9. Badge: agent slot — ` C ` / ` C×2 ` / blank. Same Max semantics.
/// 10. Badge: shell slot — ` S ` / blank. Cell carries a leading space
///    so the two badges visually separate when both present.
/// 11. Status pill — ` MERGED  ` / ` REVIEW   CI FAIL ` / blank.
///    Right-aligned. Cell is empty (width 0) when both review + CI
///    pills are None, so the column collapses for an all-empty table
///    instead of always reserving 19 cells of dead air.
/// 12. Time — ` Xm` / ` Xh` / ` Xd`, right-aligned. Leading space is
///    baked into the cell so a 1-cell gap separates time from
///    whatever sits to its left (status pill or, when status is
///    empty, the title flex padding).
pub fn build_columns(max_pr_num_width: usize) -> Vec<Column> {
    vec![
        Column::fixed(4),                        // 0: prefix
        Column::fixed(1),                        // 1: type glyph (single cell, flush against #N)
        Column::fixed(max_pr_num_width).right(), // 2: pr_num (right-aligned so digits line up)
        Column::fixed(2),                        // 3: role (" R" or blank)
        Column::fixed(3),                        // 4: asking (" ? " reserved)
        Column::max(0),                          // 5: kind ("[FEAT] " or blank)
        Column::flex(0),                         // 6: title
        Column::max(0),                          // 7: kill_mark
        Column::max(0).right(),                  // 8: unread
        Column::max(0),                          // 9: badge_agent
        Column::max(0),                          // 10: badge_shell (carries its own leading space)
        Column::max(0).right(),                  // 11: status
        Column::max(0).right(),                  // 12: time (carries its own leading space)
    ]
}

/// Build the Row<Cell> for a single workspace row. Fill style is
/// the row's cursor highlight (or unstyled when not under cursor),
/// applied via `Row::fill` so every column's padding inherits the
/// row's bg.
pub fn build_row(ctx: &WorkspaceRowCtx<'_>) -> Row {
    let cells = vec![
        cell_prefix(ctx),
        cell_type(ctx),
        cell_pr_num(ctx),
        cell_role(ctx),
        cell_asking(ctx),
        cell_kind(ctx),
        cell_title(ctx),
        cell_kill_mark(ctx),
        cell_unread(ctx),
        cell_badge_agent(ctx),
        cell_badge_shell(ctx),
        cell_status(ctx),
        cell_time(ctx),
    ];
    Row::new(cells).fill(ctx.row_style())
}

fn cell_prefix(ctx: &WorkspaceRowCtx<'_>) -> Cell {
    let s = if ctx.is_cursor { "  ▸ " } else { "    " };
    Cell::from_span(Span::styled(s.to_string(), ctx.row_style()))
}

fn cell_type(ctx: &WorkspaceRowCtx<'_>) -> Cell {
    let Some(glyph) = ctx
        .workspace
        .and_then(|w| workspace_type_label(w, ctx.ascii_glyphs))
    else {
        return Cell::empty();
    };
    let style = if ctx.is_cursor {
        ctx.row_style()
    } else {
        Style::default()
            .fg(ctx.theme.text_dim)
            .add_modifier(Modifier::BOLD)
    };
    // Single cell, no trailing space — the glyph sits flush against
    // the `#NNN` cell that follows so the row reads `⇄#312` instead
    // of `[PR]   #312` (issue #42). `glyph` is `&'static str` so the
    // Span borrows it without allocating on the per-frame hot path.
    Cell::from_span(Span::styled(glyph, style))
}

fn cell_pr_num(ctx: &WorkspaceRowCtx<'_>) -> Cell {
    let Some(n) = ctx.task.and_then(crate::components::task_label::pr_number) else {
        return Cell::empty();
    };
    let label = format!("#{n}");
    let style = if ctx.is_cursor {
        ctx.row_style()
    } else {
        Style::default()
            .fg(crate::components::task_label::pr_number_color(n))
            .add_modifier(Modifier::BOLD)
    };
    // Padding to `max_pr_num_width` happens here (not in the column)
    // because the trailing space should inherit the colored
    // background of the PR number row — but in practice the
    // `pr_number_color` only colors the digits, so the padding is
    // row-style spaces. The Table column is Fixed(max_pr_num_width),
    // so any deficit is auto-padded by the renderer using the row's
    // fill_style. We emit just the `#NNN` span here.
    Cell::from_span(Span::styled(label, style))
}

fn cell_role(ctx: &WorkspaceRowCtx<'_>) -> Cell {
    let Some(role) = ctx.task.map(|t| t.role) else {
        return Cell::empty();
    };
    let (letter, color) = role_badge(ctx.theme, role);
    let style = if ctx.is_cursor {
        ctx.row_style()
    } else {
        Style::default().fg(color).add_modifier(Modifier::BOLD)
    };
    // " R" — leading space separator + colored letter. Reads
    // cleaner than `#7204R` (which scanned as one weird token).
    Cell::new(vec![
        Span::styled(" ".to_string(), ctx.row_style()),
        Span::styled(letter.to_string(), style),
    ])
}

fn cell_asking(ctx: &WorkspaceRowCtx<'_>) -> Cell {
    if ctx.asking {
        let style = if ctx.is_cursor {
            ctx.row_style()
        } else {
            Style::default()
                .fg(ctx.theme.warn)
                .add_modifier(Modifier::BOLD)
        };
        // Reserved 3 cells: " ? " (leading + glyph + trailing space).
        Cell::new(vec![
            Span::styled(" ?".to_string(), style),
            Span::styled(" ".to_string(), ctx.row_style()),
        ])
    } else {
        Cell::empty()
    }
}

fn cell_kind(ctx: &WorkspaceRowCtx<'_>) -> Cell {
    let raw = ctx.raw_title();
    let Some((kind, _)) = crate::components::task_label::parse_conventional_prefix(raw) else {
        return Cell::empty();
    };
    let style = if ctx.is_cursor {
        ctx.row_style()
    } else {
        Style::default()
            .fg(crate::components::task_label::kind_color(kind))
            .add_modifier(Modifier::BOLD)
    };
    Cell::new(vec![
        Span::styled(format!("[{}]", kind.label()), style),
        Span::styled(" ".to_string(), ctx.row_style()),
    ])
}

fn cell_title(ctx: &WorkspaceRowCtx<'_>) -> Cell {
    let raw = ctx.raw_title();
    let body = match crate::components::task_label::parse_conventional_prefix(raw) {
        Some((_, rest)) => rest,
        None => raw,
    };
    // No truncation here — the table renderer trims with `…` when
    // the flex column ends up smaller than the cell's natural width.
    Cell::from_span(Span::styled(body.to_string(), ctx.row_style()))
}

fn cell_kill_mark(ctx: &WorkspaceRowCtx<'_>) -> Cell {
    let text = if ctx.long_snooze_armed {
        " [snooze 1y?]"
    } else {
        return Cell::empty();
    };
    // Kill mark text is theme.error fg with the row's bg behind it.
    // Style only carries fg — bg falls through from the row.
    Cell::from_span(Span::styled(
        text.to_string(),
        Style::default().fg(ctx.theme.error),
    ))
}

fn cell_unread(ctx: &WorkspaceRowCtx<'_>) -> Cell {
    let unread = ctx.workspace.map(|w| w.unread_count()).unwrap_or(0);
    if unread == 0 {
        return Cell::empty();
    }
    let text = if unread < 10 {
        format!(" ●{unread} ")
    } else if unread < 100 {
        format!(" ●{unread}")
    } else {
        " ●99+".to_string()
    };
    let style = if ctx.is_cursor {
        ctx.row_style()
    } else {
        Style::default()
            .fg(ctx.theme.hover)
            .add_modifier(Modifier::BOLD)
    };
    Cell::from_span(Span::styled(text, style))
}

/// Agent-letter pill — pulled from `ctx.badges` (the first
/// non-`S` entry). Always 3 cells wide; blank when no agent
/// running. Multi-instance (` C×2 `) widens by 2 cells.
fn cell_badge_agent(ctx: &WorkspaceRowCtx<'_>) -> Cell {
    let agent = ctx.badges.iter().find(|(c, _)| *c != 'S').copied();
    badge_slot_cell(ctx, agent)
}

fn cell_badge_shell(ctx: &WorkspaceRowCtx<'_>) -> Cell {
    let shell = ctx.badges.iter().find(|(c, _)| *c == 'S').copied();
    badge_slot_cell(ctx, shell)
}

fn badge_slot_cell(ctx: &WorkspaceRowCtx<'_>, badge: Option<(char, usize)>) -> Cell {
    match badge {
        Some((letter, n)) => {
            let label = if n > 1 {
                format!(" {letter}×{n} ")
            } else {
                format!(" {letter} ")
            };
            Cell::from_span(Span::styled(label, badge_pill_style(ctx.theme, letter)))
        }
        None => Cell::empty(),
    }
}

/// Width of each status pill slot — review (9) + CI (9), matching the
/// label widths in `status_pills`. Lifted to a `const` so the
/// blank-slot span doesn't have to `" ".repeat(9)` on every call.
const BLANK_PILL: &str = "         ";

fn cell_status(ctx: &WorkspaceRowCtx<'_>) -> Cell {
    let Some(task) = ctx.task else {
        return Cell::empty();
    };
    let (primary, secondary) = status_pills(task);
    // Empty cell when there's nothing to show — `Column::max(0)`
    // collapses the column across the whole table when NO row has
    // a pill, handing the slack back to the title flex. When ANY
    // row has a pill, the column expands to 19 cells (9 review + 1
    // gutter + 9 CI) and pill-less rows get padded by the table
    // renderer.
    if primary.is_none() && secondary.is_none() {
        return Cell::empty();
    }
    let row_style = ctx.row_style();
    let pill_span = |pill: Option<StatusPill>| match pill {
        Some(p) => Span::styled(p.label, p.style),
        None => Span::styled(BLANK_PILL, row_style),
    };
    Cell::new(vec![
        pill_span(primary),
        Span::styled(" ", row_style),
        pill_span(secondary),
    ])
}

fn cell_time(ctx: &WorkspaceRowCtx<'_>) -> Cell {
    let Some(task) = ctx.task else {
        return Cell::empty();
    };
    let text = crate::components::sidebar::relative_time(task.updated_at, ctx.now);
    let style = if ctx.is_cursor {
        ctx.row_style()
    } else {
        Style::default().fg(ctx.theme.text_dim)
    };
    // Time text may be `now` (3), `5m` (2), `12h` (3), `2d` (2),
    // `12mo` (4). Leading space is part of the cell so there's
    // always a 1-cell gap between time and whatever sits to its
    // left (status pill, or — when status collapses to 0 — the
    // title flex padding).
    Cell::new(vec![
        Span::styled(" ", ctx.row_style()),
        Span::styled(text, style),
    ])
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{TimeZone, Utc};
    use pilot_core::{CiStatus, ReviewStatus, Task, TaskId, TaskRole, TaskState, Workspace};

    fn fixed_time() -> chrono::DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 4, 1, 12, 0, 0).unwrap()
    }

    fn make_task(key: &str, title: &str) -> Task {
        Task {
            id: TaskId {
                source: "github".into(),
                key: key.into(),
            },
            title: title.into(),
            body: None,
            state: TaskState::Open,
            role: TaskRole::Author,
            ci: CiStatus::None,
            review: ReviewStatus::None,
            checks: vec![],
            unread_count: 0,
            url: format!("https://github.com/{key}"),
            repo: Some("owner/repo".into()),
            branch: Some("main".into()),
            base_branch: None,
            updated_at: fixed_time(),
            labels: vec![],
            reviewers: vec![],
            assignees: vec![],
            auto_merge_enabled: false,
            is_in_merge_queue: false,
            mergeable: pilot_core::Mergeable::Mergeable,
            is_behind_base: false,
            node_id: None,
            needs_reply: false,
            last_commenter: None,
            recent_activity: vec![],
            additions: 0,
            deletions: 0,
            closes_issues: vec![],
        }
    }

    fn ctx_for<'a>(
        workspace: &'a Workspace,
        task: &'a Task,
        theme: &'a Theme,
    ) -> WorkspaceRowCtx<'a> {
        WorkspaceRowCtx {
            workspace: Some(workspace),
            task: Some(task),
            theme,
            now: fixed_time(),
            focused: true,
            is_cursor: false,
            max_pr_num_width: 4,
            long_snooze_armed: false,
            asking: false,
            badges: vec![],
            ascii_glyphs: false,
        }
    }

    fn theme() -> Theme {
        crate::theme::current().clone()
    }

    #[test]
    fn build_columns_have_expected_count_and_order() {
        let cols = build_columns(5);
        assert_eq!(cols.len(), 13);
        // Title column (idx 6) is the only Flex one.
        let flex_indices: Vec<_> = cols
            .iter()
            .enumerate()
            .filter(|(_, c)| matches!(c.width, crate::components::table::ColumnWidth::Flex { .. }))
            .map(|(i, _)| i)
            .collect();
        assert_eq!(flex_indices, vec![6]);
    }

    #[test]
    fn build_columns_pr_num_uses_max_pr_num_width() {
        let cols = build_columns(7);
        match cols[2].width {
            crate::components::table::ColumnWidth::Fixed(w) => assert_eq!(w, 7),
            other => panic!("expected Fixed, got {other:?}"),
        }
    }

    /// PR-number cell prints `#NNN` with no padding — column width
    /// supplies the padding so every row aligns.
    #[test]
    fn cell_pr_num_emits_hash_number_only() {
        let task = make_task("owner/repo#42", "x");
        let ws = Workspace::from_task(task.clone(), fixed_time());
        let theme = theme();
        let ctx = ctx_for(&ws, &task, &theme);
        let cell = cell_pr_num(&ctx);
        assert_eq!(cell.spans.len(), 1);
        assert_eq!(cell.spans[0].content.as_ref(), "#42");
    }

    /// Asking glyph: when not asking, cell is empty so the column's
    /// reserved width fills with row-style spaces (no jitter).
    #[test]
    fn cell_asking_empty_when_not_asking() {
        let task = make_task("owner/repo#1", "x");
        let ws = Workspace::from_task(task.clone(), fixed_time());
        let theme = theme();
        let ctx = ctx_for(&ws, &task, &theme);
        let cell = cell_asking(&ctx);
        assert_eq!(cell.width(), 0);
    }

    /// Asking glyph: 3 cells reserved (" ?" + trailing space).
    #[test]
    fn cell_asking_three_cells_when_asking() {
        let task = make_task("owner/repo#1", "x");
        let ws = Workspace::from_task(task.clone(), fixed_time());
        let theme = theme();
        let mut ctx = ctx_for(&ws, &task, &theme);
        ctx.asking = true;
        let cell = cell_asking(&ctx);
        assert_eq!(cell.width(), 3);
    }

    /// Build a PR-shaped task — `make_task` fills `url` from `key`,
    /// so the `/pull/` segment is what makes `Workspace::attach_task`
    /// classify it as a PR (not an issue). The two `cell_type` tests
    /// need this; the existing `make_task` keys (`owner/repo#1`)
    /// would land in the gh_issues slot and render `○`.
    fn pr_task(repo: &str, n: u64) -> Task {
        let mut task = make_task(&format!("{repo}#{n}"), "x");
        task.url = format!("https://github.com/{repo}/pull/{n}");
        task
    }

    /// Type cell renders the single-cell unicode glyph by default.
    /// Anchors the layout contract for issue #42: type column is
    /// exactly 1 cell wide so `#NNN` sits flush against the glyph.
    #[test]
    fn cell_type_emits_single_cell_glyph_for_pr() {
        let task = pr_task("owner/repo", 1);
        let ws = Workspace::from_task(task.clone(), fixed_time());
        let theme = theme();
        let ctx = ctx_for(&ws, &task, &theme);
        let cell = cell_type(&ctx);
        assert_eq!(cell.width(), 1);
        assert_eq!(cell.spans[0].content.as_ref(), "⇄");
    }

    /// `ascii_glyphs = true` (config opt-in) swaps the unicode glyph
    /// for the plain letter so fonts that don't render the unicode
    /// reliably still get a usable, single-cell marker.
    #[test]
    fn cell_type_honors_ascii_fallback() {
        let task = pr_task("owner/repo", 1);
        let ws = Workspace::from_task(task.clone(), fixed_time());
        let theme = theme();
        let mut ctx = ctx_for(&ws, &task, &theme);
        ctx.ascii_glyphs = true;
        let cell = cell_type(&ctx);
        assert_eq!(cell.width(), 1);
        assert_eq!(cell.spans[0].content.as_ref(), "p");
    }

    /// Issue workspace (no PR slot) renders the `○` glyph — pins
    /// the per-variant routing through `workspace_type_label`.
    #[test]
    fn cell_type_emits_circle_for_issue() {
        let task = make_task("owner/repo#1", "x"); // make_task URL → issue
        let ws = Workspace::from_task(task.clone(), fixed_time());
        let theme = theme();
        let ctx = ctx_for(&ws, &task, &theme);
        let cell = cell_type(&ctx);
        assert_eq!(cell.width(), 1);
        assert_eq!(cell.spans[0].content.as_ref(), "○");
    }

    /// Empty workspace (no PR, no issues) → no type cell, so the
    /// glyph column collapses to nothing rather than rendering a
    /// stray character.
    #[test]
    fn cell_type_empty_for_scratch_workspace() {
        let ws = Workspace::empty(
            pilot_core::WorkspaceKey("scratch".into()),
            "main",
            fixed_time(),
        );
        let theme = theme();
        let ctx = WorkspaceRowCtx {
            workspace: Some(&ws),
            task: None,
            theme: &theme,
            now: fixed_time(),
            focused: false,
            is_cursor: false,
            max_pr_num_width: 2,
            long_snooze_armed: false,
            asking: false,
            badges: vec![],
            ascii_glyphs: false,
        };
        assert_eq!(cell_type(&ctx).width(), 0);
    }

    /// Kind label parses `feat: foo` into a `[feat] ` cell.
    #[test]
    fn cell_kind_strips_conventional_prefix() {
        let task = make_task("owner/repo#1", "feat: add login");
        let ws = Workspace::from_task(task.clone(), fixed_time());
        let theme = theme();
        let ctx = ctx_for(&ws, &task, &theme);
        let cell = cell_kind(&ctx);
        let joined: String = cell.spans.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(joined, "[FEAT] ");
    }

    /// Title cell renders the body without the conventional prefix.
    #[test]
    fn cell_title_strips_conventional_prefix() {
        let task = make_task("owner/repo#1", "feat: add login");
        let ws = Workspace::from_task(task.clone(), fixed_time());
        let theme = theme();
        let ctx = ctx_for(&ws, &task, &theme);
        let cell = cell_title(&ctx);
        assert_eq!(cell.spans[0].content.as_ref(), "add login");
    }

    /// Cursor row gets `row_focused` style and propagates via the
    /// Row's fill_style.
    #[test]
    fn build_row_cursor_gets_focused_fill_style() {
        let task = make_task("owner/repo#1", "x");
        let ws = Workspace::from_task(task.clone(), fixed_time());
        let theme = theme();
        let mut ctx = ctx_for(&ws, &task, &theme);
        ctx.is_cursor = true;
        ctx.focused = true;
        let row = build_row(&ctx);
        assert_eq!(row.fill_style, Some(theme.row_focused()));
    }

    /// Unfocused cursor row uses `row_unfocused` not `row_focused`.
    #[test]
    fn build_row_cursor_unfocused_gets_unfocused_fill_style() {
        let task = make_task("owner/repo#1", "x");
        let ws = Workspace::from_task(task.clone(), fixed_time());
        let theme = theme();
        let mut ctx = ctx_for(&ws, &task, &theme);
        ctx.is_cursor = true;
        ctx.focused = false;
        let row = build_row(&ctx);
        assert_eq!(row.fill_style, Some(theme.row_unfocused()));
    }

    /// Long-snooze armed wins over no-latch (and trumps kill in the
    /// "neither armed" case via empty return).
    #[test]
    fn cell_kill_mark_renders_long_snooze_when_armed() {
        let task = make_task("owner/repo#1", "x");
        let ws = Workspace::from_task(task.clone(), fixed_time());
        let theme = theme();
        let mut ctx = ctx_for(&ws, &task, &theme);
        ctx.long_snooze_armed = true;
        let cell = cell_kill_mark(&ctx);
        assert_eq!(cell.spans[0].content.as_ref(), " [snooze 1y?]");
    }

    /// No badges, no agent cell content.
    #[test]
    fn cell_badge_agent_empty_when_no_agent() {
        let task = make_task("owner/repo#1", "x");
        let ws = Workspace::from_task(task.clone(), fixed_time());
        let theme = theme();
        let ctx = ctx_for(&ws, &task, &theme);
        assert_eq!(cell_badge_agent(&ctx).width(), 0);
    }

    /// Single agent: ` C ` (3 cells), shell slot picks up `S` too.
    #[test]
    fn cell_badge_agent_renders_single_letter_pill() {
        let task = make_task("owner/repo#1", "x");
        let ws = Workspace::from_task(task.clone(), fixed_time());
        let theme = theme();
        let mut ctx = ctx_for(&ws, &task, &theme);
        ctx.badges = vec![('C', 1), ('S', 1)];
        assert_eq!(cell_badge_agent(&ctx).width(), 3);
        assert_eq!(cell_badge_shell(&ctx).width(), 3);
    }

    /// Multi-instance agent widens the slot to 5 cells (` C×2 `).
    #[test]
    fn cell_badge_agent_widens_for_multi_instance() {
        let task = make_task("owner/repo#1", "x");
        let ws = Workspace::from_task(task.clone(), fixed_time());
        let theme = theme();
        let mut ctx = ctx_for(&ws, &task, &theme);
        ctx.badges = vec![('C', 2)];
        assert_eq!(cell_badge_agent(&ctx).width(), 5);
    }

    /// Empty workspace (no task): title falls back to workspace name.
    #[test]
    fn cell_title_falls_back_to_workspace_name_when_no_task() {
        let ws = Workspace::empty(
            pilot_core::WorkspaceKey("lonely".into()),
            "main",
            fixed_time(),
        );
        let theme = theme();
        let ctx = WorkspaceRowCtx {
            workspace: Some(&ws),
            task: None,
            theme: &theme,
            now: fixed_time(),
            focused: false,
            is_cursor: false,
            max_pr_num_width: 3,
            long_snooze_armed: false,
            asking: false,
            badges: vec![],
            ascii_glyphs: false,
        };
        assert_eq!(cell_title(&ctx).spans[0].content.as_ref(), "lonely");
    }

    /// `cell_status` returns an empty cell when neither review nor CI
    /// has anything to surface — so the table's `Column::max(0)` for
    /// status collapses to 0 when no row in the visible list has a
    /// pill, handing the slack back to the title flex.
    #[test]
    fn cell_status_empty_when_no_pills() {
        let mut task = make_task("owner/repo#1", "x");
        task.review = ReviewStatus::None;
        task.ci = CiStatus::None;
        task.state = TaskState::Open;
        let ws = Workspace::from_task(task.clone(), fixed_time());
        let theme = theme();
        let ctx = ctx_for(&ws, &task, &theme);
        assert_eq!(cell_status(&ctx).width(), 0);
    }

    /// When a status pill IS present the cell stays 19 cells wide —
    /// review (9) + sep (1) + CI (9) — so rows with one pill line up
    /// alongside rows with two.
    #[test]
    fn cell_status_is_nineteen_cells_with_a_pill() {
        let mut task = make_task("owner/repo#1", "x");
        task.ci = CiStatus::Failure;
        let ws = Workspace::from_task(task.clone(), fixed_time());
        let theme = theme();
        let ctx = ctx_for(&ws, &task, &theme);
        assert_eq!(cell_status(&ctx).width(), 19);
    }

    /// `cell_time` carries its own leading space, so when the status
    /// column collapses (no pills anywhere) the time still reads as
    /// `<title flex padding>` + 1-cell gap + `5m`, not jammed against
    /// the title's last character.
    #[test]
    fn cell_time_emits_leading_space() {
        let task = make_task("owner/repo#1", "x");
        let ws = Workspace::from_task(task.clone(), fixed_time());
        let theme = theme();
        let ctx = ctx_for(&ws, &task, &theme);
        let cell = cell_time(&ctx);
        // First span is a single-cell row-style space.
        assert_eq!(cell.spans[0].content.as_ref(), " ");
    }

    /// Regression for issue #22, part 1: title flex reclaims the
    /// 36+ trailing cells when no row has unread / badge / status
    /// content. The pre-fix table reserved 5 (unread) + 7 (badges) +
    /// 19 (status) + 1 (gutter) = 32 cells PER ROW even when every
    /// cell was empty, so a 100-cell sidebar effectively gave the
    /// title flex only ~42 cells and truncated long titles with `…`
    /// while leaving a huge gap to the right.
    #[test]
    fn title_flex_expands_when_trailing_columns_are_all_empty() {
        // Two open issues, both with NO unread / badges / CI / review.
        let task_a = make_task(
            "owner/repo#1",
            "Round-robin per-repo sync to reduce query overhead",
        );
        let task_b = make_task(
            "owner/repo#2",
            "Switch from polling-driven sync to notifications",
        );
        let ws_a = Workspace::from_task(task_a.clone(), fixed_time());
        let ws_b = Workspace::from_task(task_b.clone(), fixed_time());
        let theme = theme();
        let ctx_a = ctx_for(&ws_a, &task_a, &theme);
        let ctx_b = ctx_for(&ws_b, &task_b, &theme);
        let columns = build_columns(4);
        let rows = vec![build_row(&ctx_a), build_row(&ctx_b)];
        // Generous row budget (mimics a wide terminal's sidebar).
        let lines = crate::components::table::render_table(&rows, &columns, 100);
        // Both lines render. The title text should make it into the
        // line (truncated or not) — the bug was the title getting
        // chopped to ~42 cells with 30+ empty cells trailing it.
        let joined_a: String = lines[0].spans.iter().map(|s| s.content.as_ref()).collect();
        let joined_b: String = lines[1].spans.iter().map(|s| s.content.as_ref()).collect();
        // Title body should appear in full when there's room. With
        // no trailing-column content, the longest natural title here
        // (`Round-robin per-repo sync to reduce query overhead`,
        // 50 cells) fits comfortably inside the 100-cell budget.
        assert!(
            joined_a.contains("Round-robin per-repo sync to reduce query overhead"),
            "title was truncated despite empty trailing columns: {joined_a:?}",
        );
        assert!(
            joined_b.contains("Switch from polling-driven sync to notifications"),
            "title was truncated despite empty trailing columns: {joined_b:?}",
        );
    }

    /// Regression for issue #22, part 2: a row WITHOUT a `C` badge
    /// (and no other right-side content) does not leave the badge
    /// column as a ragged gap. When at least one row has a `C`
    /// badge, every other row pads to the same column width so the
    /// `C` letters line up at the same x position across rows.
    #[test]
    fn badge_column_lines_up_across_rows() {
        // Row A: has a Claude agent badge. Row B: no badge.
        let task_a = make_task("owner/repo#1", "A");
        let task_b = make_task("owner/repo#2", "B");
        let ws_a = Workspace::from_task(task_a.clone(), fixed_time());
        let ws_b = Workspace::from_task(task_b.clone(), fixed_time());
        let theme = theme();
        let mut ctx_a = ctx_for(&ws_a, &task_a, &theme);
        ctx_a.badges = vec![('C', 1)];
        let ctx_b = ctx_for(&ws_b, &task_b, &theme);
        let columns = build_columns(4);
        let rows = vec![build_row(&ctx_a), build_row(&ctx_b)];
        let lines = crate::components::table::render_table(&rows, &columns, 80);
        let row_a: String = lines[0].spans.iter().map(|s| s.content.as_ref()).collect();
        let row_b: String = lines[1].spans.iter().map(|s| s.content.as_ref()).collect();
        // Same total visible width across rows — that's what makes
        // every fixed-position column (incl. the trailing time
        // column) align across rows.
        let width_a = crate::util::visual_width(&row_a);
        let width_b = crate::util::visual_width(&row_b);
        assert_eq!(
            width_a, width_b,
            "rows must render to the same total width or trailing columns drift: {row_a:?} vs {row_b:?}",
        );
        // Find the `C` glyph in row A; row B must have a space at
        // the SAME char/cell offset, not anything else. The type
        // glyph (`⇄` / `○`) is multi-byte UTF-8, so anchor by char
        // position — `str::find` is byte-based and would drift.
        let row_a_chars: Vec<char> = row_a.chars().collect();
        let c_pos = row_a_chars
            .windows(3)
            .position(|w| w == [' ', 'C', ' '])
            .expect("row A should have a ` C ` badge");
        let same_window: String = row_b.chars().skip(c_pos).take(3).collect();
        assert_eq!(
            same_window, "   ",
            "row B did not reserve the ` C ` column at the same x as row A",
        );
    }

    /// Multi-instance badge (` C×2 `, 5 cells) no longer gets
    /// truncated to `… ` because the badge column is `Column::max(0)`
    /// and expands to the widest natural cell across the table.
    #[test]
    fn multi_instance_badge_is_not_truncated() {
        let task = make_task("owner/repo#1", "x");
        let ws = Workspace::from_task(task.clone(), fixed_time());
        let theme = theme();
        let mut ctx = ctx_for(&ws, &task, &theme);
        ctx.badges = vec![('C', 2)];
        let columns = build_columns(4);
        let rows = vec![build_row(&ctx)];
        let lines = crate::components::table::render_table(&rows, &columns, 80);
        let line: String = lines[0].spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(
            line.contains(" C×2 "),
            "multi-instance badge was truncated: {line:?}",
        );
    }
}
