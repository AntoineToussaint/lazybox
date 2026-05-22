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
    BADGE_COL_W, STATUS_COL_W, TIME_COL_W, UNREAD_COL_W, badge_pill_style, role_badge, status_pill,
    workspace_type_label,
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
/// Built once (with `max_pr_num_width` from the pre-pass), used for
/// every row's `render_table` call.
///
/// Order (left → right):
///
/// 0.  Prefix — `  ▸ ` (cursor) / `    ` (no cursor).
/// 1.  Type label — `[PR] ` / `[I ] ` / blank.
/// 2.  PR number — `#NNN`, padded to `max_pr_num_width`.
/// 3.  Role badge — ` R` colored marker, or blank.
/// 4.  Asking glyph — ` ? ` warn-colored, or blank — reserved width
///     so the kind/title to the right don't jitter between
///     asking / not-asking rows.
/// 5.  Kind label — `[feat] ` etc, or blank.
/// 6.  Title — flex, absorbs the remaining width. Truncates with `…`.
/// 7.  Kill mark — ` [kill?]` / ` [snooze 1y?]`, or blank.
/// 8.  Unread pill — ` ●N `, right-aligned.
/// 9.  Badge: agent slot — ` C ` / ` C×2 ` / blank.
/// 10. Badge: separator — single space.
/// 11. Badge: shell slot — ` S ` / blank.
/// 12. Status pill — ` MERGED   ` etc, right-aligned.
/// 13. Gutter — single space between status + time.
/// 14. Time — `Xm` / `Xh` / `Xd`, right-aligned.
pub fn build_columns(max_pr_num_width: usize) -> Vec<Column> {
    // The badge column is split into THREE slots so the agent / shell
    // letters land at fixed x positions across rows: ` X ` (3) + sep
    // (1) + ` X ` (3) = BADGE_COL_W.
    debug_assert_eq!(BADGE_COL_W, 3 + 1 + 3);
    vec![
        Column::fixed(4),                        // 0: prefix
        Column::fixed(5),                        // 1: type label (incl. trailing space)
        Column::fixed(max_pr_num_width).right(), // 2: pr_num (right-aligned so digits line up)
        Column::fixed(2),                        // 3: role (" R" or blank)
        Column::fixed(3),                        // 4: asking (" ? " reserved)
        Column::max(0),                          // 5: kind ("[feat] " or blank)
        Column::flex(0),                         // 6: title
        Column::max(0),                          // 7: kill_mark
        Column::fixed(UNREAD_COL_W).right(),     // 8: unread
        Column::fixed(3),                        // 9: badge_agent
        Column::fixed(1),                        // 10: badge_sep
        Column::fixed(3),                        // 11: badge_shell
        Column::fixed(STATUS_COL_W).right(),     // 12: status
        Column::fixed(1),                        // 13: gutter
        Column::fixed(TIME_COL_W).right(),       // 14: time
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
        cell_badge_sep(ctx),
        cell_badge_shell(ctx),
        cell_status(ctx),
        cell_gutter(ctx),
        cell_time(ctx),
    ];
    Row::new(cells).fill(ctx.row_style())
}

fn cell_prefix(ctx: &WorkspaceRowCtx<'_>) -> Cell {
    let s = if ctx.is_cursor { "  ▸ " } else { "    " };
    Cell::from_span(Span::styled(s.to_string(), ctx.row_style()))
}

fn cell_type(ctx: &WorkspaceRowCtx<'_>) -> Cell {
    let Some(tag) = ctx.workspace.and_then(workspace_type_label) else {
        return Cell::empty();
    };
    let style = if ctx.is_cursor {
        ctx.row_style()
    } else {
        Style::default()
            .fg(ctx.theme.text_dim)
            .add_modifier(Modifier::BOLD)
    };
    // `tag` is "[PR]" or "[I ]" (4 cells). Trailing space pulls the
    // PR number off the bracket so the columns visually separate.
    Cell::from_span(Span::styled(format!("{tag} "), style))
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

fn cell_badge_sep(ctx: &WorkspaceRowCtx<'_>) -> Cell {
    // Single space so the two badge pills don't visually touch.
    Cell::from_span(Span::styled(" ".to_string(), ctx.row_style()))
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

fn cell_status(ctx: &WorkspaceRowCtx<'_>) -> Cell {
    let Some(pill) = ctx.task.and_then(status_pill) else {
        return Cell::empty();
    };
    // The pill label is already padded to STATUS_COL_W (see
    // `pill_for_tag`), so the cell's width equals the column's
    // width and no extra padding is added.
    Cell::from_span(Span::styled(pill.label.to_string(), pill.style))
}

fn cell_gutter(ctx: &WorkspaceRowCtx<'_>) -> Cell {
    // Visible 1-cell separator between status pill and time. Empty
    // when there's no task (so no status pill either) — but the
    // gutter still reserves its cell so the time column anchors
    // identically across rows.
    Cell::from_span(Span::styled(" ".to_string(), ctx.row_style()))
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
    // `12mo` (4). Right-aligned column pads on the left.
    Cell::from_span(Span::styled(text, style))
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
            has_conflicts: false,
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
        }
    }

    fn theme() -> Theme {
        crate::theme::current().clone()
    }

    #[test]
    fn build_columns_have_expected_count_and_order() {
        let cols = build_columns(5);
        assert_eq!(cols.len(), 15);
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
        };
        assert_eq!(cell_title(&ctx).spans[0].content.as_ref(), "lonely");
    }
}
