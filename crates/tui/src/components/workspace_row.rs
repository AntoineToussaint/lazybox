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
    /// Any agent in this workspace is in `AgentState::InputNeeded`.
    /// Renders the `?` pill in the shared state slot. Mutually
    /// exclusive with `working` — input-needed wins if both were ever
    /// set (they can't be, by the disjoint asking/working sets).
    pub asking: bool,
    /// Any agent in this workspace is in `AgentState::Working`
    /// (streaming / running a tool). Renders the animated spinner in
    /// the same slot the `?` pill uses.
    pub working: bool,
    /// Current spinner glyph for the `working` slot. Shared across all
    /// rows in a render pass — the sidebar advances a single frame
    /// counter on a low-rate tick (see `Sidebar::tick_working`), so
    /// the animation costs one glyph lookup per working row, no
    /// per-tick row rebuild.
    pub working_glyph: &'static str,
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
///    Exactly 1 cell so it sits flush against the `NNN` to its right
///    — see issue #42.
/// 2. PR number — `NNN` (no `#` prefix; the glyph carries the type —
///    issue #67), left-aligned and padded to `max_pr_num_width` so the
///    digits sit FLUSH against the type glyph (`⇄312`, `○7`) on every
///    row. Right-aligning instead pushed shorter numbers off the glyph
///    with leading padding (`⇄312` vs `○  7`) — the inconsistent
///    post-glyph spacing of issue #65. Trailing padding still aligns
///    the role column to a fixed x across rows.
/// 3. Role badge — ` R` colored marker, or blank.
/// 4. State slot — ` ? ` (input-needed, warn) / ` ⠋ ` (working,
///    animated accent spinner) / blank (idle). One slot, three
///    mutually-exclusive states; reserved width so the title to the
///    right doesn't jitter as a row changes state.
/// 5. Title — flex, absorbs the remaining width. Truncates with `…`.
///    Conventional-commit / bracket tags like `[CI]` stay inline at
///    the front of the title rather than being hoisted into a
///    reserved column that every tag-less row would pay for (#80).
/// 6. Labels — ` [bug] [ci] +2`, or blank. Max so the title flex
///    reclaims the space when no row has labels; truncates at 3
///    chips with a `+N` overflow indicator.
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
        Column::fixed(4),                // 0: prefix
        Column::fixed(1),                // 1: type glyph (single cell, flush against num)
        Column::fixed(max_pr_num_width), // 2: pr_num (left-aligned, flush against the glyph)
        Column::fixed(2),                // 3: role (" R" or blank)
        Column::fixed(3),                // 4: state slot (" ? "/" ⠋ "/blank, reserved)
        Column::flex(0),                 // 5: title
        Column::max(0),                  // 6: labels
        Column::max(0),                  // 7: kill_mark
        Column::max(0).right(),          // 8: unread
        Column::max(0),                  // 9: badge_agent
        Column::max(0),                  // 10: badge_shell (carries its own leading space)
        Column::max(0).right(),          // 11: status
        Column::max(0).right(),          // 12: time (carries its own leading space)
    ]
}

/// Build the `Row<Cell>` for a single workspace row. Fill style is
/// the row's cursor highlight (or unstyled when not under cursor),
/// applied via `Row::fill` so every column's padding inherits the
/// row's bg.
pub fn build_row(ctx: &WorkspaceRowCtx<'_>) -> Row {
    let cells = vec![
        cell_prefix(ctx),
        cell_type(ctx),
        cell_pr_num(ctx),
        cell_role(ctx),
        cell_state(ctx),
        cell_title(ctx),
        cell_labels(ctx),
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
    // the `NNN` cell that follows so the row reads `⇄312` instead
    // of `[PR]   #312` (issues #42, #67). `glyph` is `&'static str`
    // so the Span borrows it without allocating on the per-frame hot
    // path.
    Cell::from_span(Span::styled(glyph, style))
}

fn cell_pr_num(ctx: &WorkspaceRowCtx<'_>) -> Cell {
    let Some(n) = ctx.task.and_then(crate::components::task_label::pr_number) else {
        return Cell::empty();
    };
    let label = format!("{n}");
    let style = if ctx.is_cursor {
        ctx.row_style()
    } else {
        Style::default()
            .fg(crate::components::task_label::pr_number_color(n))
            .add_modifier(Modifier::BOLD)
    };
    // No `#` prefix: the type glyph in the column to the left already
    // says "issue" (`○`) or "PR" (`⇄`), so the `#` was redundant and
    // cost a column on every row (issue #67). We emit just the `NNN`
    // span; the column is Fixed(max_pr_num_width) and LEFT-aligned, so
    // the renderer pads the deficit on the RIGHT with the row's
    // fill_style. Left alignment keeps the number flush against the
    // type glyph on every row (issue #65) — a right-aligned column
    // padded shorter numbers on the left, opening an inconsistent gap
    // after the glyph. `pr_number_color` colors the digits.
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
    // cleaner than `7204R` (which scanned as one weird token).
    Cell::new(vec![
        Span::styled(" ".to_string(), ctx.row_style()),
        Span::styled(letter.to_string(), style),
    ])
}

/// The shared per-session state slot: a single 3-cell column that
/// renders the agent's current `AgentState` with a distinct visual
/// per state, so it's one thing to scan:
///   - `InputNeeded` → ` ? ` (warn, bold) — a static glyph: the
///     agent is paused waiting on me.
///   - `Working`     → ` <spinner> ` (accent, bold) — an animated
///     glyph: the agent is making progress right now.
///   - `Idle`        → blank.
/// Reserved width either way so the kind/title to the right don't
/// jitter as a row moves between states. InputNeeded takes precedence
/// over Working defensively, though the two are disjoint upstream.
fn cell_state(ctx: &WorkspaceRowCtx<'_>) -> Cell {
    let (glyph, fg) = if ctx.asking {
        ("?", ctx.theme.warn)
    } else if ctx.working {
        (ctx.working_glyph, ctx.theme.accent)
    } else {
        return Cell::empty();
    };
    let style = if ctx.is_cursor {
        ctx.row_style()
    } else {
        Style::default().fg(fg).add_modifier(Modifier::BOLD)
    };
    // Reserved 3 cells: " G " (leading + glyph + trailing space).
    // `glyph` is `&'static str` and the spaces are literals, so every
    // span borrows static data — no per-row, per-frame allocation on
    // the render hot path.
    Cell::new(vec![
        Span::styled(" ", style),
        Span::styled(glyph, style),
        Span::styled(" ", ctx.row_style()),
    ])
}

/// Spinner frames for the "working" state slot. A small braille
/// cycle — visually distinct from the static `?` input-needed glyph
/// and cheap to render. `working_glyph` indexes this by the sidebar's
/// shared frame counter.
pub(crate) const WORKING_SPINNER_FRAMES: &[&str] =
    &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

/// Resolve the spinner glyph for a given frame index. Wraps, so the
/// caller's counter can grow unbounded.
pub(crate) fn working_glyph(frame: usize) -> &'static str {
    WORKING_SPINNER_FRAMES[frame % WORKING_SPINNER_FRAMES.len()]
}

fn cell_title(ctx: &WorkspaceRowCtx<'_>) -> Cell {
    // The full title, tags and all. Bracketed tags like `[CI]` stay
    // where they originated instead of being hoisted into a reserved
    // column (#80). No truncation here — the table renderer trims with
    // `…` when the flex column ends up smaller than the cell's natural
    // width.
    Cell::from_span(Span::styled(ctx.raw_title().to_string(), ctx.row_style()))
}

/// Render the task's labels as compact chips: ` [name] [name] +N`.
/// Caps at 3 chips with a `+N` overflow indicator so the row layout
/// stays predictable when a PR has many labels. Each chip's text
/// adopts the GitHub label color (parsed from the hex string) as
/// the foreground; falls back to `text_dim` for the bracket
/// delimiters so the bracket framing reads consistently across the
/// rainbow.
fn cell_labels(ctx: &WorkspaceRowCtx<'_>) -> Cell {
    const MAX_CHIPS: usize = 3;
    let labels = match ctx.task.map(|t| t.labels.as_slice()) {
        Some(ls) if !ls.is_empty() => ls,
        _ => return Cell::empty(),
    };
    let total = labels.len();
    let shown = labels.iter().take(MAX_CHIPS);
    // Upper bound: MAX_CHIPS chips × (space + `[` + name + `]`) +
    // one optional overflow span. Sized to the visible rendering,
    // not the input length — a PR with 50 labels still only emits
    // 13 spans worth of buffer here.
    let mut spans: Vec<Span<'static>> = Vec::with_capacity(MAX_CHIPS * 4 + 1);
    for label in shown {
        spans.push(Span::styled(" ".to_string(), ctx.row_style()));
        let bracket_style = if ctx.is_cursor {
            ctx.row_style()
        } else {
            Style::default().fg(ctx.theme.text_dim)
        };
        let text_style = if ctx.is_cursor {
            ctx.row_style()
        } else {
            label_text_style(ctx.theme, &label.color)
        };
        spans.push(Span::styled("[", bracket_style));
        spans.push(Span::styled(label.name.clone(), text_style));
        spans.push(Span::styled("]", bracket_style));
    }
    if total > MAX_CHIPS {
        let overflow_style = if ctx.is_cursor {
            ctx.row_style()
        } else {
            Style::default().fg(ctx.theme.text_dim)
        };
        spans.push(Span::styled(
            format!(" +{}", total - MAX_CHIPS),
            overflow_style,
        ));
    }
    Cell::new(spans)
}

/// Translate GitHub's hex color (e.g. `"d73a4a"`) into a ratatui
/// `Style`. Empty / unparseable → `text_dim`. The hex string may
/// arrive with or without a leading `#`; both shapes are handled.
///
/// ASCII-gated before byte-slicing: `.len()` is byte length, not
/// char count, so without the gate a 2-byte UTF-8 char that happens
/// to fit in 6 bytes would slice through a code point and panic.
/// GitHub never returns that, but providers are external input.
fn label_text_style(theme: &Theme, hex: &str) -> Style {
    let cleaned = hex.trim_start_matches('#');
    if !cleaned.is_ascii() || cleaned.len() != 6 {
        return Style::default().fg(theme.text_dim);
    }
    let parse = |s: &str| u8::from_str_radix(s, 16).ok();
    match (
        parse(&cleaned[0..2]),
        parse(&cleaned[2..4]),
        parse(&cleaned[4..6]),
    ) {
        (Some(r), Some(g), Some(b)) => Style::default().fg(ratatui::style::Color::Rgb(r, g, b)),
        _ => Style::default().fg(theme.text_dim),
    }
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
            working: false,
            working_glyph: working_glyph(0),
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
        // Title column (idx 5) is the only Flex one.
        let flex_indices: Vec<_> = cols
            .iter()
            .enumerate()
            .filter(|(_, c)| matches!(c.width, crate::components::table::ColumnWidth::Flex { .. }))
            .map(|(i, _)| i)
            .collect();
        assert_eq!(flex_indices, vec![5]);
    }

    #[test]
    fn build_columns_pr_num_uses_max_pr_num_width() {
        let cols = build_columns(7);
        match cols[2].width {
            crate::components::table::ColumnWidth::Fixed(w) => assert_eq!(w, 7),
            other => panic!("expected Fixed, got {other:?}"),
        }
    }

    /// PR-number cell prints `NNN` (no `#` prefix — issue #67) with no
    /// padding; the column width supplies the padding so every row
    /// aligns. The type glyph to the left now carries the
    /// issue-vs-PR signal the `#` used to.
    #[test]
    fn cell_pr_num_emits_bare_number_only() {
        let task = make_task("owner/repo#42", "x");
        let ws = Workspace::from_task(task.clone(), fixed_time());
        let theme = theme();
        let ctx = ctx_for(&ws, &task, &theme);
        let cell = cell_pr_num(&ctx);
        assert_eq!(cell.spans.len(), 1);
        assert_eq!(cell.spans[0].content.as_ref(), "42");
    }

    /// State slot: idle (neither asking nor working) → empty cell, so
    /// the column's reserved width fills with row-style spaces (no
    /// jitter as rows change state).
    #[test]
    fn cell_state_empty_when_idle() {
        let task = make_task("owner/repo#1", "x");
        let ws = Workspace::from_task(task.clone(), fixed_time());
        let theme = theme();
        let ctx = ctx_for(&ws, &task, &theme);
        let cell = cell_state(&ctx);
        assert_eq!(cell.width(), 0);
    }

    /// Concatenate a cell's span contents — the rendered text,
    /// independent of how it's split into styled spans.
    fn cell_text(cell: &Cell) -> String {
        cell.spans.iter().map(|s| s.content.as_ref()).collect()
    }

    /// State slot: input-needed → 3 cells (" ? " — leading space,
    /// glyph, trailing space).
    #[test]
    fn cell_state_three_cells_when_asking() {
        let task = make_task("owner/repo#1", "x");
        let ws = Workspace::from_task(task.clone(), fixed_time());
        let theme = theme();
        let mut ctx = ctx_for(&ws, &task, &theme);
        ctx.asking = true;
        let cell = cell_state(&ctx);
        assert_eq!(cell.width(), 3);
        assert_eq!(cell_text(&cell), " ? ");
    }

    /// State slot: working → 3 cells with the current spinner glyph.
    /// Same reserved width as the asking pill so the slot never jitters.
    #[test]
    fn cell_state_three_cells_with_spinner_when_working() {
        let task = make_task("owner/repo#1", "x");
        let ws = Workspace::from_task(task.clone(), fixed_time());
        let theme = theme();
        let mut ctx = ctx_for(&ws, &task, &theme);
        ctx.working = true;
        ctx.working_glyph = working_glyph(3);
        let cell = cell_state(&ctx);
        assert_eq!(cell.width(), 3);
        assert_eq!(cell_text(&cell), format!(" {} ", working_glyph(3)));
    }

    /// State slot precedence: input-needed wins over working if both
    /// flags are somehow set (they can't be, by the disjoint sets,
    /// but the slot must still render exactly one thing).
    #[test]
    fn cell_state_input_needed_wins_over_working() {
        let task = make_task("owner/repo#1", "x");
        let ws = Workspace::from_task(task.clone(), fixed_time());
        let theme = theme();
        let mut ctx = ctx_for(&ws, &task, &theme);
        ctx.asking = true;
        ctx.working = true;
        let cell = cell_state(&ctx);
        assert_eq!(cell_text(&cell), " ? ");
    }

    /// The spinner frame index wraps so an unbounded counter is safe.
    #[test]
    fn working_glyph_wraps_frame_index() {
        let n = WORKING_SPINNER_FRAMES.len();
        assert_eq!(working_glyph(0), working_glyph(n));
        assert_eq!(working_glyph(1), working_glyph(n + 1));
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
            working: false,
            working_glyph: working_glyph(0),
            badges: vec![],
            ascii_glyphs: false,
        };
        assert_eq!(cell_type(&ctx).width(), 0);
    }

    /// Title cell keeps a bracketed `[CI]`-style tag inline instead of
    /// hoisting it into a reserved column (#80).
    #[test]
    fn cell_title_keeps_bracket_tag_inline() {
        let task = make_task("owner/repo#1", "[CI] cache post-job upload");
        let ws = Workspace::from_task(task.clone(), fixed_time());
        let theme = theme();
        let ctx = ctx_for(&ws, &task, &theme);
        let cell = cell_title(&ctx);
        assert_eq!(cell.spans[0].content.as_ref(), "[CI] cache post-job upload");
    }

    /// Title cell keeps a conventional-commit prefix inline rather than
    /// stripping it into a separate kind column (#80).
    #[test]
    fn cell_title_keeps_conventional_prefix_inline() {
        let task = make_task("owner/repo#1", "feat: add login");
        let ws = Workspace::from_task(task.clone(), fixed_time());
        let theme = theme();
        let ctx = ctx_for(&ws, &task, &theme);
        let cell = cell_title(&ctx);
        assert_eq!(cell.spans[0].content.as_ref(), "feat: add login");
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
            working: false,
            working_glyph: working_glyph(0),
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

    /// Labels render as bracketed chips with one leading space per
    /// chip. Empty label list → empty cell so the column collapses
    /// to 0 when no row in the table has labels.
    #[test]
    fn cell_labels_empty_for_taskless_or_unlabeled_row() {
        let task = make_task("owner/repo#1", "x");
        let ws = Workspace::from_task(task.clone(), fixed_time());
        let theme = theme();
        let ctx = ctx_for(&ws, &task, &theme);
        assert_eq!(cell_labels(&ctx).width(), 0);
    }

    #[test]
    fn cell_labels_renders_bracketed_chips() {
        let mut task = make_task("owner/repo#1", "x");
        task.labels = vec![pilot_core::Label::new("bug"), pilot_core::Label::new("ci")];
        let ws = Workspace::from_task(task.clone(), fixed_time());
        let theme = theme();
        let ctx = ctx_for(&ws, &task, &theme);
        let cell = cell_labels(&ctx);
        let joined: String = cell.spans.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(joined, " [bug] [ci]");
    }

    /// More than 3 labels collapses extras into a `+N` overflow
    /// indicator — the issue's "graceful truncation" requirement.
    #[test]
    fn cell_labels_truncates_with_overflow_indicator() {
        let mut task = make_task("owner/repo#1", "x");
        task.labels = vec![
            pilot_core::Label::new("bug"),
            pilot_core::Label::new("ci"),
            pilot_core::Label::new("backend"),
            pilot_core::Label::new("priority"),
            pilot_core::Label::new("docs"),
        ];
        let ws = Workspace::from_task(task.clone(), fixed_time());
        let theme = theme();
        let ctx = ctx_for(&ws, &task, &theme);
        let cell = cell_labels(&ctx);
        let joined: String = cell.spans.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(joined, " [bug] [ci] [backend] +2");
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

    /// Regression for issue #65: the type glyph must sit FLUSH against
    /// the `NNN` on every row, regardless of how wide that row's
    /// number is. The bug was a right-aligned pr-number column: it
    /// padded shorter numbers on the LEFT, so `⇄312` rendered flush
    /// while `○7` picked up leading spaces (`○  7`) — inconsistent
    /// post-glyph spacing across rows. Left alignment moves the
    /// padding to the right, keeping the glyph→number gap a constant
    /// zero cells everywhere. (The `#` prefix itself was dropped in
    /// issue #67; the glyph now carries the issue-vs-PR signal.)
    #[test]
    fn type_glyph_is_flush_against_number_for_mixed_widths() {
        let theme = theme();
        // Mixed glyph types AND mixed number widths (1/2/3 digits) so
        // `max_pr_num_width` is driven by `312` (3 cells). The narrower
        // rows are the ones the old right-align padded on the left; the
        // issue (`○`) row exercises the non-PR glyph path too, so a
        // regression can't hide on one glyph variant.
        let cases: [(Task, &str); 3] = [
            (make_task("owner/repo#7", "x"), "○7"), // issue, 1 digit
            (pr_task("owner/repo", 42), "⇄42"),     // PR, 2 digits
            (pr_task("owner/repo", 312), "⇄312"),   // PR, 3 digits
        ];
        let workspaces: Vec<Workspace> = cases
            .iter()
            .map(|(task, _)| Workspace::from_task(task.clone(), fixed_time()))
            .collect();
        let ctxs: Vec<WorkspaceRowCtx<'_>> = workspaces
            .iter()
            .zip(cases.iter())
            .map(|(ws, (task, _))| ctx_for(ws, task, &theme))
            .collect();
        let columns = build_columns(3); // width of "312"
        let rows: Vec<Row> = ctxs.iter().map(build_row).collect();
        let lines = crate::components::table::render_table(&rows, &columns, 80);
        for (line, (_, expected)) in lines.iter().zip(cases.iter()) {
            let joined: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
            assert!(
                joined.contains(expected),
                "type glyph must sit flush against the number (no gap); \
                 wanted {expected:?} in {joined:?}",
            );
        }
    }

    /// Companion to the above for the "without a task" case in the
    /// acceptance criteria: a scratch workspace (no PR / issue / linear
    /// ticket) renders the same total width as task rows so the title
    /// and trailing columns stay aligned across the mixed list — the
    /// empty type / number cells are padded by their fixed columns, not
    /// dropped.
    #[test]
    fn taskless_row_keeps_alignment_with_task_rows() {
        let theme = theme();
        let task = pr_task("owner/repo", 312);
        let ws_task = Workspace::from_task(task.clone(), fixed_time());
        let ctx_task = ctx_for(&ws_task, &task, &theme);

        let ws_scratch = Workspace::empty(
            pilot_core::WorkspaceKey("scratch-branch".into()),
            "main",
            fixed_time(),
        );
        let ctx_scratch = WorkspaceRowCtx {
            workspace: Some(&ws_scratch),
            task: None,
            theme: &theme,
            now: fixed_time(),
            focused: false,
            is_cursor: false,
            max_pr_num_width: 4,
            long_snooze_armed: false,
            asking: false,
            working: false,
            working_glyph: working_glyph(0),
            badges: vec![],
            ascii_glyphs: false,
        };
        let columns = build_columns(4);
        let rows = vec![build_row(&ctx_task), build_row(&ctx_scratch)];
        let lines = crate::components::table::render_table(&rows, &columns, 80);
        let task_line: String = lines[0].spans.iter().map(|s| s.content.as_ref()).collect();
        let scratch_line: String = lines[1].spans.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(
            crate::util::visual_width(&task_line),
            crate::util::visual_width(&scratch_line),
            "taskless row must render to the same width as a task row: {task_line:?} vs {scratch_line:?}",
        );
    }
}
