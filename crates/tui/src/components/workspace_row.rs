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
    badge_pill_style, role_badge, status_pills, workspace_type_label,
};
use crate::components::table::{Cell, Column, Row};
use crate::theme::Theme;
use lazybox_core::{Task, Workspace};
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
    /// This workspace is in the broadcast multi-select set (`v`).
    /// Renders a `✓` in the shared selection gutter; on the cursor row
    /// the caret keeps the slot but goes accent so the row still reads
    /// as selected.
    pub is_selected: bool,
    /// Widest `#NNN` across all visible workspace rows in this
    /// render pass. Every row's pr-number cell pads to this width
    /// so the role / asking columns line up across rows.
    pub max_pr_num_width: usize,
    /// Any agent in this workspace is in `AgentState::InputNeeded`.
    /// Renders the `?` pill in the shared state slot. Mutually
    /// exclusive with `working` — input-needed wins if both were ever
    /// set (they can't be, by the disjoint asking/working sets).
    pub asking: bool,
    /// Any agent in this workspace is in `AgentState::Working`
    /// (streaming / running a tool). Renders the animated spinner in
    /// the same slot the `?` pill uses.
    pub working: bool,
    /// Any agent in this workspace is in `AgentState::Done` — finished
    /// its turn, waiting to be looked at (#80). Renders `✓` in the same
    /// slot. Mutually exclusive with `asking`/`working` upstream;
    /// asking and working take precedence defensively if both were set.
    pub done: bool,
    /// The workspace's agent process has exited (`AgentState::Exited` —
    /// clean or crash; #356/#357). Renders `✗` in the same slot so a dead
    /// agent reads as "ended, restart it" instead of blanking to nothing.
    /// Lowest precedence: a live signal (asking/working/done) always wins.
    pub exited: bool,
    /// Current spinner glyph for the `working` slot. Shared across all
    /// rows in a render pass — the sidebar advances a single frame
    /// counter on a low-rate tick (see `Sidebar::tick_working`), so
    /// the animation costs one glyph lookup per working row, no
    /// per-tick row rebuild.
    pub working_glyph: &'static str,
    /// `Sidebar::runner_badges(key)` — `[('C', n), ('S', m)]` etc.
    pub badges: Vec<(char, usize)>,
    /// This workspace's 1-based jump number — its slot in the
    /// sidebar-order agent roster (`Sidebar::agent_workspace_keys`).
    /// `Some` only for workspaces with a coding agent; rendered as a
    /// small badge ahead of the agent pill so the user can see which
    /// `]]<digit>` lands here. `None` for non-agent rows (and for
    /// agents past the 9th, which have no single-digit jump).
    pub agent_number: Option<usize>,
    /// Render the type indicator as plain ASCII (`p`/`i`/`l`) instead
    /// of the default unicode glyphs (`⇄`/`○`/`◆`). Wired from
    /// `display.ascii_glyphs` in `~/.lazybox/config.yaml`.
    pub ascii_glyphs: bool,
    /// This workspace has "auto-merge on green" armed
    /// (`Workspace::auto_merge_on_green`). Renders a distinct ` ARM `
    /// pill ahead of the status pills so the user can see, at a glance,
    /// which rows will merge themselves once CI goes green.
    pub auto_merge_armed: bool,
    /// This workspace has an auto-fix behavior explicitly armed
    /// (`Workspace::policies` — issue #363). Renders a ` FIX ` pill so an
    /// explicit per-session auto-fix arm is visible, never invisible.
    pub auto_fix_armed: bool,
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

    /// An issue that's been open long enough to read in months (`Nmo`
    /// in the time column). Age is measured from when it was opened, not
    /// last touched, so an old issue with recent chatter still reads as
    /// old. PRs are excluded — they carry their own staleness cues (CI,
    /// review, conflict pills) and a fade would fight those. Stale issues
    /// get a dim title so active rows stand out and old ones don't waste
    /// a second glance (issue #274).
    fn is_stale_issue(&self) -> bool {
        self.task.is_some_and(|t| {
            !t.is_pr() && lazybox_core::time::is_stale_at(&t.opened_at(), self.now)
        })
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
/// 0. Prefix — `▸` (cursor) / ` ` (no cursor). A single shared
///    selection gutter: the caret occupies one column reused across
///    every row type, instead of a 2-col `▸ ` re-added at each depth
///    (issue #231). Rows sit one column in from the repo header's
///    disclosure arrow, so the tree nesting still reads.
/// 1. Type glyph — `⇄` / `○` / `◆` (or ASCII `p`/`i`/`l`) / blank,
///    followed by a single space separator (2 cells total) so the
///    row reads `⇄ 312` rather than the cramped `⇄312` — see issues
///    #42 and #94.
/// 2. PR number — `NNN` (no `#` prefix; the glyph carries the type —
///    issue #67), left-aligned and padded to `max_pr_num_width` so the
///    digits sit one space off the type glyph (`⇄ 312`, `○ 7`) on every
///    row. Right-aligning instead pushed shorter numbers off the glyph
///    with leading padding (`⇄ 312` vs `○   7`) — the inconsistent
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
///    reserved column that every tag-less row would pay for (#80). The
///    task's labels (` [bug] [ci] +2`) ride at the tail of this same
///    cell — as an atomic droppable group — instead of a reserved
///    column. A global `Max` label column made every row, tag-less
///    ones included, reserve the widest label cell anywhere in the
///    sidebar, so one Dependabot repo's ` [deps] [go]` chips truncated
///    unrelated tag-less titles (#329). Inline, a label-less row hands
///    all that width to its title; the chips are excluded from the
///    flex's protected floor, so under width pressure they shed whole
///    (after the status pill — #328) before the title elides.
/// 6. Unread pill — ` ●N `, right-aligned. Max so the column collapses
///    when no row has unread, and lines up at a consistent x when any
///    row does.
/// 7. Badge: agent slot — ` C ` / ` C×2 ` / blank. Same Max semantics.
/// 8. Badge: shell slot — ` S ` / blank. Cell carries a leading space
///    so the two badges visually separate when both present.
/// 9. Status pill — ` MERGED ` / ` REVIEW  CI FAIL ` / blank.
///    Right-aligned, sized to the pills actually present (each pill is
///    trimmed to its own ` LABEL ` block — no blank-slot filler), so a
///    lone CI pill sits one clean gap off the time. Cell is empty
///    (width 0) when both review + CI pills are None, so the column
///    collapses for an all-empty table.
/// 10. Time — ` Xm` / ` Xh` / ` Xd`, right-aligned. Leading space is
///    baked into the cell so a 1-cell gap separates time from
///    whatever sits to its left (status pill or, when status is
///    empty, the title flex padding).
pub fn build_columns(max_pr_num_width: usize) -> Vec<Column> {
    // Drop order when the sidebar is too narrow to fit every column:
    // lower priority sheds first. The issue number + title (and the
    // type glyph that tells issue-from-PR) are kept. Labels are the
    // least important thing on the row; they ride in the title cell as
    // an atomic tail (excluded from the flex floor), so they shed
    // before any of these columns — and, crucially, before the status
    // pill (CI / CONFLICT — the actionable signal), which is kept
    // nearly as long as the title (issue #328).
    const P_TIME: u8 = 10;
    const P_UNREAD: u8 = 30;
    const P_BADGE_SHELL: u8 = 40;
    const P_BADGE_AGENT: u8 = 50;
    const P_ROLE: u8 = 60;
    const P_STATE: u8 = 70;
    const P_STATUS: u8 = 80;
    // The title should keep at least this many cells before any
    // secondary column is allowed to crowd it out — below this a
    // title is just a word fragment + `…` and tells you nothing.
    const TITLE_MIN: usize = 20;
    vec![
        Column::fixed(1),                          // 0: prefix (shared 1-col caret gutter)
        Column::fixed(2),                          // 1: type glyph + trailing space separator
        Column::fixed(max_pr_num_width), // 2: pr_num (left-aligned, one space off the glyph)
        Column::fixed(2).priority(P_ROLE), // 3: role (" R" or blank)
        Column::fixed(3).priority(P_STATE), // 4: state slot (" ? "/" ⠋ "/blank, reserved)
        Column::flex(TITLE_MIN),         // 5: title (labels ride inline at its tail)
        Column::max(0).right().priority(P_UNREAD), // 6: unread
        Column::max(0).priority(P_BADGE_AGENT), // 7: badge_agent
        Column::max(0).priority(P_BADGE_SHELL), // 8: badge_shell (carries its own leading space)
        Column::max(0).right().priority(P_STATUS), // 9: status
        Column::max(0).right().priority(P_TIME), // 10: time (carries its own leading space)
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
        cell_unread(ctx),
        cell_badge_agent(ctx),
        cell_badge_shell(ctx),
        cell_status(ctx),
        cell_time(ctx),
    ];
    Row::new(cells).fill(ctx.row_style())
}

fn cell_prefix(ctx: &WorkspaceRowCtx<'_>) -> Cell {
    let s = if ctx.is_cursor {
        "▸"
    } else if ctx.is_selected {
        "✓"
    } else {
        " "
    };
    let style = if ctx.is_selected {
        ctx.row_style()
            .fg(ctx.theme.accent)
            .add_modifier(Modifier::BOLD)
    } else {
        ctx.row_style()
    };
    Cell::from_span(Span::styled(s, style))
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
    // Glyph + a single trailing space so the row reads `⇄ 312`
    // instead of the cramped `⇄312` (issue #94); the space separator
    // also keeps the `#`-less number readable (issues #42, #67).
    // Both spans borrow `&'static str` (no per-frame allocation on the
    // hot path); the trailing space takes the row fill style like the
    // other inter-cell separators.
    Cell::new(vec![
        Span::styled(glyph, style),
        Span::styled(" ", ctx.row_style()),
    ])
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
    // fill_style. Left alignment keeps the number one space off the
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
        Span::styled(" ", ctx.row_style()),
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
///   - `Done`        → ` ✓ ` (success, bold) — a static glyph: the
///     agent finished its turn and is waiting to be looked at (#80).
///   - `Exited`      → ` ✗ ` (dim) — a static glyph: the agent process
///     ended (clean or crash; #356/#357). Not an alert color — a dead
///     agent is a fact to notice, not an emergency.
///   - `Idle`        → blank.
/// Reserved width either way so the kind/title to the right don't
/// jitter as a row moves between states. Precedence asking > working >
/// done > exited, applied defensively though the states are disjoint
/// upstream (a live signal always wins over the terminal exit marker).
fn cell_state(ctx: &WorkspaceRowCtx<'_>) -> Cell {
    let (glyph, fg) = if ctx.asking {
        ("?", ctx.theme.warn)
    } else if ctx.working {
        (ctx.working_glyph, ctx.theme.accent)
    } else if ctx.done {
        ("✓", ctx.theme.success)
    } else if ctx.exited {
        ("✗", ctx.theme.text_dim)
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
    //
    // Stale issues fade (DIM) so the eye skips them — but never on the
    // cursor row, whose highlight fill must stay legible.
    let mut style = ctx.row_style();
    if !ctx.is_cursor && ctx.is_stale_issue() {
        style = style.add_modifier(Modifier::DIM);
    }
    // Labels ride at the tail of the title cell rather than in a
    // reserved column (#329): a tag-less row hands all that width to
    // its title. Marked as the cell's atomic tail so they shed as one
    // unit — after the status pill (#328), never sliced mid-chip —
    // when the row is too narrow (see `Cell::atomic_tail`).
    let labels = label_spans(ctx);
    let tail = labels.len();
    let mut spans = vec![Span::styled(ctx.raw_title().to_string(), style)];
    spans.extend(labels);
    Cell::new(spans).atomic_tail(tail)
}

/// Hard cap on a single chip's text (before the `…`). A verbose
/// label like `github_actions` otherwise eats a big slice of the row;
/// past this we truncate with an ellipsis so no one chip dominates.
const MAX_CHIP_LEN: usize = 10;

/// Shorten a label name for its chip: a small alias table for the
/// common verbose GitHub labels (`dependencies` → `deps`), then a hard
/// per-chip length cap with a trailing `…` for everything else
/// (issue #328). Case-insensitive on the alias lookup so `Dependencies`
/// and `dependencies` collapse the same way.
fn abbreviate_label(name: &str) -> String {
    let alias = match name.to_ascii_lowercase().as_str() {
        "dependencies" => Some("deps"),
        "documentation" => Some("docs"),
        "enhancement" => Some("enhance"),
        _ => None,
    };
    if let Some(short) = alias {
        return short.to_string();
    }
    if name.chars().count() > MAX_CHIP_LEN {
        let head: String = name.chars().take(MAX_CHIP_LEN - 1).collect();
        format!("{head}…")
    } else {
        name.to_string()
    }
}

/// Render the task's labels as compact chips: ` [name] [name] +N`.
/// Caps at 3 chips with a `+N` overflow indicator so the row layout
/// stays predictable when a PR has many labels. Each chip's text
/// adopts the GitHub label color (parsed from the hex string) as
/// the foreground; falls back to `text_dim` for the bracket
/// delimiters so the bracket framing reads consistently across the
/// rainbow. Empty when the row has no labels — the caller
/// (`cell_title`) then emits nothing at the title's tail.
///
/// Now that the chips ride in the title cell (#329), a stale issue's
/// fade covers them too: the title dims to send the eye elsewhere, so
/// full-color chips beside it would fight that cue. Suppressed on the
/// cursor row, matching `cell_title`.
fn label_spans(ctx: &WorkspaceRowCtx<'_>) -> Vec<Span<'static>> {
    const MAX_CHIPS: usize = 3;
    let labels = match ctx.task.map(|t| t.labels.as_slice()) {
        Some(ls) if !ls.is_empty() => ls,
        _ => return Vec::new(),
    };
    let dim = !ctx.is_cursor && ctx.is_stale_issue();
    let maybe_dim = |style: Style| {
        if dim {
            style.add_modifier(Modifier::DIM)
        } else {
            style
        }
    };
    let total = labels.len();
    let shown = labels.iter().take(MAX_CHIPS);
    // Upper bound: MAX_CHIPS chips × (space + `[` + name + `]`) +
    // one optional overflow span. Sized to the visible rendering,
    // not the input length — a PR with 50 labels still only emits
    // 13 spans worth of buffer here.
    let mut spans: Vec<Span<'static>> = Vec::with_capacity(MAX_CHIPS * 4 + 1);
    for label in shown {
        spans.push(Span::styled(" ", ctx.row_style()));
        let bracket_style = if ctx.is_cursor {
            ctx.row_style()
        } else {
            maybe_dim(Style::default().fg(ctx.theme.text_dim))
        };
        let text_style = if ctx.is_cursor {
            ctx.row_style()
        } else {
            maybe_dim(label_text_style(ctx.theme, &label.color))
        };
        spans.push(Span::styled("[", bracket_style));
        spans.push(Span::styled(abbreviate_label(&label.name), text_style));
        spans.push(Span::styled("]", bracket_style));
    }
    if total > MAX_CHIPS {
        let overflow_style = if ctx.is_cursor {
            ctx.row_style()
        } else {
            maybe_dim(Style::default().fg(ctx.theme.text_dim))
        };
        spans.push(Span::styled(
            format!(" +{}", total - MAX_CHIPS),
            overflow_style,
        ));
    }
    spans
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
/// non-`S` entry). Blank when no agent running; multi-instance
/// (` C×2 `) widens by 2 cells. When the row carries a jump number
/// (`agent_number`), a dim ` N` prefix rides ahead of the pill —
/// ` 2 C ` — so the user can see which `]]<digit>` lands here.
fn cell_badge_agent(ctx: &WorkspaceRowCtx<'_>) -> Cell {
    let agent = ctx.badges.iter().find(|(c, _)| *c != 'S').copied();
    match (agent, ctx.agent_number) {
        (Some((letter, n)), Some(num)) => {
            let count = if n > 1 {
                format!("×{n}")
            } else {
                String::new()
            };
            let num_style = if ctx.is_cursor {
                ctx.row_style()
            } else {
                Style::default()
                    .fg(ctx.theme.text_dim)
                    .add_modifier(Modifier::BOLD)
            };
            Cell::new(vec![
                Span::styled(format!(" {num}"), num_style),
                Span::styled(
                    format!(" {letter}{count} "),
                    badge_pill_style(ctx.theme, letter),
                ),
            ])
        }
        _ => badge_slot_cell(ctx, agent),
    }
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
    let Some(task) = ctx.task else {
        return Cell::empty();
    };
    let (primary, secondary) = status_pills(task);
    // Empty cell when there's nothing to show — `Column::max(0)`
    // collapses the column across the whole table when NO row has a
    // pill, handing the slack back to the title flex. An armed row
    // always shows its ` ARM ` marker even when no CI/review pill
    // applies yet (e.g. armed before CI runs).
    if primary.is_none() && secondary.is_none() && !ctx.auto_merge_armed && !ctx.auto_fix_armed {
        return Cell::empty();
    }
    // Emit only the pills that are actually present, each trimmed to
    // its own ` LABEL ` block (the padding lives in the label). No
    // blank-slot filler: a pill-less side would just stack dead space
    // between the visible pill and the time trailer (issue #328).
    // Right-aligned by the column, so the rightmost pill sits one clean
    // gap off the duration — its block's trailing space plus the time
    // cell's leading space, nothing more.
    let mut spans = Vec::with_capacity(4);
    if ctx.auto_merge_armed {
        let arm_style = if ctx.is_cursor {
            ctx.row_style()
        } else {
            Style::default()
                .bg(ctx.theme.accent)
                .fg(ratatui::style::Color::Black)
                .add_modifier(Modifier::BOLD)
        };
        spans.push(Span::styled(" ARM ", arm_style));
    }
    if ctx.auto_fix_armed {
        let fix_style = if ctx.is_cursor {
            ctx.row_style()
        } else {
            Style::default()
                .bg(ctx.theme.warn)
                .fg(ratatui::style::Color::Black)
                .add_modifier(Modifier::BOLD)
        };
        spans.push(Span::styled(" FIX ", fix_style));
    }
    if let Some(p) = primary {
        spans.push(Span::styled(p.label, p.style));
    }
    if let Some(p) = secondary {
        spans.push(Span::styled(p.label, p.style));
    }
    Cell::new(spans)
}

fn cell_time(ctx: &WorkspaceRowCtx<'_>) -> Cell {
    let Some(task) = ctx.task else {
        return Cell::empty();
    };
    // A stale issue shows its age (time since opened) so it reads as old
    // even after recent chatter. Everything else — PRs, and recent issues —
    // keeps the last-activity timestamp: PRs because their CI/review pills
    // key off activity, recent issues because the issue asked to leave
    // their existing display untouched (issue #274).
    let anchor = if ctx.is_stale_issue() {
        task.opened_at()
    } else {
        task.updated_at
    };
    let text = crate::components::sidebar::relative_time(anchor, ctx.now);
    let style = if ctx.is_cursor {
        ctx.row_style()
    } else if ctx.is_stale_issue() {
        // Already `Nmo` here — fade it further so a stale issue's age
        // reads as "old, ignore me" rather than just another timestamp.
        Style::default()
            .fg(ctx.theme.text_dim)
            .add_modifier(Modifier::DIM)
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
    use lazybox_core::{CiStatus, ReviewStatus, Task, TaskId, TaskRole, TaskState, Workspace};

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
            created_at: None,
            closed_at: None,
            labels: vec![],
            reviewers: vec![],
            assignees: vec![],
            auto_merge_enabled: false,
            is_in_merge_queue: false,
            mergeable: lazybox_core::Mergeable::Mergeable,
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
            is_selected: false,
            max_pr_num_width: 4,
            asking: false,
            working: false,
            done: false,
            exited: false,
            working_glyph: working_glyph(0),
            badges: vec![],
            agent_number: None,
            ascii_glyphs: false,
            auto_merge_armed: false,
            auto_fix_armed: false,
        }
    }

    fn theme() -> Theme {
        crate::theme::current().clone()
    }

    #[test]
    fn build_columns_have_expected_count_and_order() {
        let cols = build_columns(5);
        // 11 columns: the labels column retired into the title cell
        // (#329) — a tag-less row no longer reserves the sidebar-wide
        // widest label width.
        assert_eq!(cols.len(), 11);
        // Title column (idx 5) is the only Flex one.
        let flex_indices: Vec<_> = cols
            .iter()
            .enumerate()
            .filter(|(_, c)| matches!(c.width, crate::components::table::ColumnWidth::Flex { .. }))
            .map(|(i, _)| i)
            .collect();
        assert_eq!(flex_indices, vec![5]);
    }

    /// Regression for issue #231: the row prefix is a single shared
    /// 1-cell selection gutter (`▸` / ` `), not a 2-cell `▸ ` re-added
    /// at every depth. Reclaims one column of title room on every
    /// workspace row (and #121's earlier 4→2 cut goes the rest of the
    /// way to 1).
    #[test]
    fn cell_prefix_is_single_cell_selection_gutter() {
        let task = make_task("owner/repo#1", "x");
        let ws = Workspace::from_task(task.clone(), fixed_time());
        let theme = theme();
        let mut ctx = ctx_for(&ws, &task, &theme);

        ctx.is_cursor = false;
        let cell = cell_prefix(&ctx);
        assert_eq!(cell.width(), 1);
        assert_eq!(cell_text(&cell), " ");

        ctx.is_cursor = true;
        let cell = cell_prefix(&ctx);
        assert_eq!(cell.width(), 1);
        assert_eq!(cell_text(&cell), "▸");

        // The fixed prefix column matches the cell width so the table
        // doesn't pad the inset back out.
        match build_columns(4)[0].width {
            crate::components::table::ColumnWidth::Fixed(w) => assert_eq!(w, 1),
            other => panic!("expected Fixed(1) prefix column, got {other:?}"),
        }
    }

    /// Broadcast multi-select: a selected row shows `✓` in the shared
    /// gutter; the cursor row keeps its caret (accent-styled) so the
    /// slot stays a single cell either way.
    #[test]
    fn cell_prefix_shows_check_for_selected_rows() {
        let task = make_task("owner/repo#1", "x");
        let ws = Workspace::from_task(task.clone(), fixed_time());
        let theme = theme();
        let mut ctx = ctx_for(&ws, &task, &theme);

        ctx.is_selected = true;
        let cell = cell_prefix(&ctx);
        assert_eq!(cell.width(), 1);
        assert_eq!(cell_text(&cell), "✓");
        assert_eq!(cell.spans[0].style.fg, Some(theme.accent));

        // Cursor + selected: caret wins the glyph, accent keeps the
        // selected signal.
        ctx.is_cursor = true;
        let cell = cell_prefix(&ctx);
        assert_eq!(cell_text(&cell), "▸");
        assert_eq!(cell.spans[0].style.fg, Some(theme.accent));
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

    /// State slot: done → 3 cells with a `✓`, same reserved width as
    /// the asking pill and working spinner (#80).
    #[test]
    fn cell_state_three_cells_with_check_when_done() {
        let task = make_task("owner/repo#1", "x");
        let ws = Workspace::from_task(task.clone(), fixed_time());
        let theme = theme();
        let mut ctx = ctx_for(&ws, &task, &theme);
        ctx.done = true;
        let cell = cell_state(&ctx);
        assert_eq!(cell.width(), 3);
        assert_eq!(cell_text(&cell), " ✓ ");
    }

    /// State slot: exited → 3 cells with a `✗`, same reserved width as the
    /// other pills (#356/#357).
    #[test]
    fn cell_state_three_cells_with_cross_when_exited() {
        let task = make_task("owner/repo#1", "x");
        let ws = Workspace::from_task(task.clone(), fixed_time());
        let theme = theme();
        let mut ctx = ctx_for(&ws, &task, &theme);
        ctx.exited = true;
        let cell = cell_state(&ctx);
        assert_eq!(cell.width(), 3);
        assert_eq!(cell_text(&cell), " ✗ ");
    }

    /// State slot precedence: a live signal (here `done`) wins over the
    /// terminal `exited` marker if both are somehow set.
    #[test]
    fn cell_state_done_wins_over_exited() {
        let task = make_task("owner/repo#1", "x");
        let ws = Workspace::from_task(task.clone(), fixed_time());
        let theme = theme();
        let mut ctx = ctx_for(&ws, &task, &theme);
        ctx.done = true;
        ctx.exited = true;
        let cell = cell_state(&ctx);
        assert_eq!(cell_text(&cell), " ✓ ");
    }

    /// State slot precedence: working wins over done if both flags are
    /// somehow set (disjoint upstream, but the slot renders one thing).
    #[test]
    fn cell_state_working_wins_over_done() {
        let task = make_task("owner/repo#1", "x");
        let ws = Workspace::from_task(task.clone(), fixed_time());
        let theme = theme();
        let mut ctx = ctx_for(&ws, &task, &theme);
        ctx.working = true;
        ctx.working_glyph = working_glyph(2);
        ctx.done = true;
        let cell = cell_state(&ctx);
        assert_eq!(cell_text(&cell), format!(" {} ", working_glyph(2)));
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

    /// Type cell renders the unicode glyph plus a trailing space by
    /// default. Anchors the layout contract for issues #42 and #94:
    /// type column is 2 cells (glyph + separator) so the number sits
    /// one space off the glyph.
    #[test]
    fn cell_type_emits_glyph_then_space_for_pr() {
        let task = pr_task("owner/repo", 1);
        let ws = Workspace::from_task(task.clone(), fixed_time());
        let theme = theme();
        let ctx = ctx_for(&ws, &task, &theme);
        let cell = cell_type(&ctx);
        assert_eq!(cell.width(), 2);
        assert_eq!(cell.spans[0].content.as_ref(), "⇄");
        assert_eq!(cell.spans[1].content.as_ref(), " ");
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
        assert_eq!(cell.width(), 2);
        assert_eq!(cell.spans[0].content.as_ref(), "p");
        assert_eq!(cell.spans[1].content.as_ref(), " ");
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
        assert_eq!(cell.width(), 2);
        assert_eq!(cell.spans[0].content.as_ref(), "○");
        assert_eq!(cell.spans[1].content.as_ref(), " ");
    }

    /// Empty workspace (no PR, no issues) → no type cell, so the
    /// glyph column collapses to nothing rather than rendering a
    /// stray character.
    #[test]
    fn cell_type_empty_for_scratch_workspace() {
        let ws = Workspace::empty(
            lazybox_core::WorkspaceKey("scratch".into()),
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
            is_selected: false,
            max_pr_num_width: 2,
            asking: false,
            working: false,
            done: false,
            exited: false,
            working_glyph: working_glyph(0),
            badges: vec![],
            agent_number: None,
            ascii_glyphs: false,
            auto_merge_armed: false,
            auto_fix_armed: false,
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

    /// A jump number prefixes the agent pill (` 2 C `, 5 cells) so the
    /// `]]<digit>` target is visible; the digit is the `agent_number`.
    #[test]
    fn cell_badge_agent_shows_jump_number() {
        let task = make_task("owner/repo#1", "x");
        let ws = Workspace::from_task(task.clone(), fixed_time());
        let theme = theme();
        let mut ctx = ctx_for(&ws, &task, &theme);
        ctx.badges = vec![('C', 1)];
        ctx.agent_number = Some(2);
        let cell = cell_badge_agent(&ctx);
        // ` 2`(2) + ` C `(3) = 5 cells, vs. 3 for the bare pill.
        assert_eq!(cell.width(), 5);
        let text: String = cell.spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(text.contains('2'), "jump number missing: {text:?}");
        assert!(text.contains('C'), "agent letter missing: {text:?}");
    }

    /// Without a jump number the agent pill stays at its bare ` C ` —
    /// non-agent rows and agents past the ninth get no badge.
    #[test]
    fn cell_badge_agent_no_number_stays_bare() {
        let task = make_task("owner/repo#1", "x");
        let ws = Workspace::from_task(task.clone(), fixed_time());
        let theme = theme();
        let mut ctx = ctx_for(&ws, &task, &theme);
        ctx.badges = vec![('C', 1)];
        ctx.agent_number = None;
        assert_eq!(cell_badge_agent(&ctx).width(), 3);
    }

    /// Empty workspace (no task): title falls back to workspace name.
    #[test]
    fn cell_title_falls_back_to_workspace_name_when_no_task() {
        let ws = Workspace::empty(
            lazybox_core::WorkspaceKey("lonely".into()),
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
            is_selected: false,
            max_pr_num_width: 3,
            asking: false,
            working: false,
            done: false,
            exited: false,
            working_glyph: working_glyph(0),
            badges: vec![],
            agent_number: None,
            ascii_glyphs: false,
            auto_merge_armed: false,
            auto_fix_armed: false,
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

    /// A lone CI pill is sized to just its own ` CI FAIL ` block (9
    /// cells) — no blank review-slot filler padding it out to 19 and
    /// stacking dead space before the time trailer (issue #328).
    #[test]
    fn cell_status_is_trimmed_to_the_present_pill() {
        let mut task = make_task("owner/repo#1", "x");
        task.ci = CiStatus::Failure;
        let ws = Workspace::from_task(task.clone(), fixed_time());
        let theme = theme();
        let ctx = ctx_for(&ws, &task, &theme);
        let cell = cell_status(&ctx);
        assert_eq!(cell.width(), 9);
        assert_eq!(cell.spans.len(), 1);
        assert_eq!(cell.spans[0].content.as_ref(), " CI FAIL ");
    }

    /// An armed workspace surfaces its ` ARM ` marker even when the PR
    /// has no CI / review pill yet — so a freshly-armed row is visibly
    /// distinct before CI even starts.
    #[test]
    fn cell_status_shows_arm_pill_when_armed_without_ci() {
        let mut task = make_task("owner/repo#1", "x");
        task.review = ReviewStatus::None;
        task.ci = CiStatus::None;
        task.state = TaskState::Open;
        let ws = Workspace::from_task(task.clone(), fixed_time());
        let theme = theme();
        let mut ctx = ctx_for(&ws, &task, &theme);
        ctx.auto_merge_armed = true;
        let cell = cell_status(&ctx);
        assert!(cell.width() > 0, "armed row renders a status cell");
        assert_eq!(cell.spans[0].content.as_ref(), " ARM ");
    }

    /// A workspace with an auto-fix policy explicitly armed surfaces a
    /// ` FIX ` pill (issue #363) so the per-session arm is never
    /// invisible — even before any CI pill applies.
    #[test]
    fn cell_status_shows_fix_pill_when_auto_fix_armed() {
        let mut task = make_task("owner/repo#1", "x");
        task.review = ReviewStatus::None;
        task.ci = CiStatus::None;
        task.state = TaskState::Open;
        let ws = Workspace::from_task(task.clone(), fixed_time());
        let theme = theme();
        let mut ctx = ctx_for(&ws, &task, &theme);
        ctx.auto_fix_armed = true;
        let cell = cell_status(&ctx);
        assert!(cell.width() > 0, "auto-fix-armed row renders a status cell");
        assert!(
            cell.spans.iter().any(|s| s.content.as_ref() == " FIX "),
            "FIX marker present"
        );
    }

    /// The ARM marker rides ahead of the live CI pill rather than
    /// replacing it — an armed PR with running/red CI shows both.
    #[test]
    fn cell_status_arm_pill_coexists_with_ci_pill() {
        let mut task = make_task("owner/repo#1", "x");
        task.ci = CiStatus::Failure;
        let ws = Workspace::from_task(task.clone(), fixed_time());
        let theme = theme();
        let mut ctx = ctx_for(&ws, &task, &theme);
        ctx.auto_merge_armed = true;
        let cell = cell_status(&ctx);
        assert!(
            cell.spans.iter().any(|s| s.content.as_ref() == " ARM "),
            "armed marker present"
        );
        assert!(
            cell.spans.iter().any(|s| s.content.as_ref().contains("CI")),
            "live CI pill still present alongside the arm"
        );
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

    fn make_stale_task(key: &str, title: &str) -> Task {
        let mut t = make_task(key, title);
        t.updated_at = fixed_time() - chrono::Duration::days(40);
        t
    }

    /// Issue #274, finding 1: age is measured from when the issue was
    /// opened, not last touched. An issue opened months ago but commented
    /// on today still reads as old (`Nmo`) and fades — `updated_at` alone
    /// would have shown a misleading `now`.
    #[test]
    fn old_issue_with_recent_activity_still_reads_as_stale() {
        let mut task = make_task("owner/repo#1", "old but chatty");
        task.created_at = Some(fixed_time() - chrono::Duration::days(90));
        task.updated_at = fixed_time(); // touched just now
        let ws = Workspace::from_task(task.clone(), fixed_time());
        let theme = theme();
        let ctx = ctx_for(&ws, &task, &theme);

        assert!(ctx.is_stale_issue());
        assert_eq!(cell_text(&cell_time(&ctx)).trim(), "3mo");
        assert!(
            cell_title(&ctx).spans[0]
                .style
                .add_modifier
                .contains(Modifier::DIM)
        );
    }

    /// Issue #274: a recent issue keeps its last-activity display — the
    /// age anchor only kicks in once the issue is stale, so a 3-day-old
    /// issue commented on an hour ago still reads `1h`, not `3d`.
    #[test]
    fn recent_issue_keeps_activity_display() {
        let mut task = make_task("owner/repo#1", "young and active");
        task.created_at = Some(fixed_time() - chrono::Duration::days(3));
        task.updated_at = fixed_time() - chrono::Duration::hours(1);
        let ws = Workspace::from_task(task.clone(), fixed_time());
        let theme = theme();
        let ctx = ctx_for(&ws, &task, &theme);

        assert!(!ctx.is_stale_issue());
        assert_eq!(cell_text(&cell_time(&ctx)).trim(), "1h");
        assert!(
            !cell_title(&ctx).spans[0]
                .style
                .add_modifier
                .contains(Modifier::DIM)
        );
    }

    /// Issue #274: an issue old enough to read in months fades — its
    /// title and age carry `DIM` so active rows draw the eye.
    #[test]
    fn stale_issue_title_and_time_are_dimmed() {
        let task = make_stale_task("owner/repo#1", "old issue");
        let ws = Workspace::from_task(task.clone(), fixed_time());
        let theme = theme();
        let ctx = ctx_for(&ws, &task, &theme);

        assert!(ctx.is_stale_issue());
        // Age renders in months.
        assert_eq!(cell_text(&cell_time(&ctx)).trim(), "1mo");

        let title = cell_title(&ctx);
        assert!(title.spans[0].style.add_modifier.contains(Modifier::DIM));
        let time = cell_time(&ctx);
        // The age span (after the leading space) is dimmed.
        assert!(time.spans[1].style.add_modifier.contains(Modifier::DIM));
    }

    /// A recent issue is untouched — no fade.
    #[test]
    fn fresh_issue_is_not_dimmed() {
        let task = make_task("owner/repo#1", "new issue");
        let ws = Workspace::from_task(task.clone(), fixed_time());
        let theme = theme();
        let ctx = ctx_for(&ws, &task, &theme);

        assert!(!ctx.is_stale_issue());
        let title = cell_title(&ctx);
        assert!(!title.spans[0].style.add_modifier.contains(Modifier::DIM));
    }

    /// PRs keep their own staleness cues (CI / review pills) — an old
    /// PR must NOT fade, or the fade would fight those signals.
    #[test]
    fn stale_pr_is_not_dimmed() {
        let mut task = make_stale_task("owner/repo#7", "old pr");
        task.url = "https://github.com/owner/repo/pull/7".into();
        let ws = Workspace::from_task(task.clone(), fixed_time());
        let theme = theme();
        let ctx = ctx_for(&ws, &task, &theme);

        assert!(task.is_pr());
        assert!(!ctx.is_stale_issue());
        let title = cell_title(&ctx);
        assert!(!title.spans[0].style.add_modifier.contains(Modifier::DIM));
    }

    /// A PR shows recency of activity, not age: an old PR pushed to just
    /// now reads as `now`, keeping the time column aligned with the
    /// CI/review pills that key off activity.
    #[test]
    fn pr_time_tracks_activity_not_age() {
        let mut task = make_task("owner/repo#7", "old but active pr");
        task.url = "https://github.com/owner/repo/pull/7".into();
        task.created_at = Some(fixed_time() - chrono::Duration::days(90));
        task.updated_at = fixed_time();
        let ws = Workspace::from_task(task.clone(), fixed_time());
        let theme = theme();
        let ctx = ctx_for(&ws, &task, &theme);

        assert!(task.is_pr());
        assert_eq!(cell_text(&cell_time(&ctx)).trim(), "now");
    }

    /// The cursor row's highlight fill must stay legible — the fade is
    /// suppressed there even when the issue is stale.
    #[test]
    fn cursor_row_suppresses_stale_fade() {
        let task = make_stale_task("owner/repo#1", "old issue");
        let ws = Workspace::from_task(task.clone(), fixed_time());
        let theme = theme();
        let mut ctx = ctx_for(&ws, &task, &theme);
        ctx.is_cursor = true;

        let title = cell_title(&ctx);
        assert!(!title.spans[0].style.add_modifier.contains(Modifier::DIM));
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

    /// Regression for issue #130: a wide pane keeps every column —
    /// the narrow-width shedding must NOT kick in when everything
    /// fits. Status pill, time, and the full title all render.
    #[test]
    fn wide_width_keeps_status_time_and_title() {
        let mut task = make_task("owner/repo#42", "Fix the broken sidebar layout");
        task.ci = CiStatus::Failure; // " CI FAIL " status pill
        let ws = Workspace::from_task(task.clone(), fixed_time());
        let theme = theme();
        let ctx = ctx_for(&ws, &task, &theme);
        let columns = build_columns(2);
        let rows = vec![build_row(&ctx)];
        let lines = crate::components::table::render_table(&rows, &columns, 100);
        let line: String = lines[0].spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(line.contains("42"), "number missing on wide pane: {line:?}");
        assert!(
            line.contains("Fix the broken sidebar layout"),
            "title truncated on wide pane: {line:?}",
        );
        assert!(
            line.contains("CI FAIL"),
            "status pill missing on wide pane: {line:?}"
        );
    }

    /// Regression for issue #328: at a narrow width the CI status is
    /// KEPT — it's the actionable signal, so it sheds nearly last —
    /// while the timestamp is the first column to go. (Before the
    /// shed-priority swap the status pill dropped out ahead of the
    /// less-important columns, exactly backwards.)
    #[test]
    fn narrow_width_keeps_status_and_sheds_time_first() {
        let mut task = make_task("owner/repo#42", "Fix the broken sidebar layout");
        task.ci = CiStatus::Failure;
        task.updated_at = fixed_time() - chrono::Duration::minutes(5); // " 5m"
        let ws = Workspace::from_task(task.clone(), fixed_time());
        let theme = theme();
        let ctx = ctx_for(&ws, &task, &theme);
        let columns = build_columns(2);
        let rows = vec![build_row(&ctx)];
        let lines = crate::components::table::render_table(&rows, &columns, 40);
        let line: String = lines[0].spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(
            line.contains("42"),
            "item number dropped at narrow width: {line:?}"
        );
        assert!(
            line.contains("Fix the broken"),
            "title squeezed to nothing at narrow width: {line:?}",
        );
        assert!(
            line.contains("CI FAIL"),
            "status must survive — it now sheds nearly last: {line:?}",
        );
        assert!(
            !line.contains("5m"),
            "the timestamp should shed first at this width: {line:?}",
        );
    }

    /// Regression for issue #269: narrowing the sidebar must not drop
    /// the CI status column while the title is short enough to leave
    /// room for it. The old budgeter reserved the title's full 20-cell
    /// `min` and shed the status column to fund padding it never filled
    /// — losing the CI signal AND leaving a blank gap. A 7-cell title
    /// at width 44 has ample room for the 19-cell status column.
    #[test]
    fn short_title_keeps_status_instead_of_leaving_empty_gap() {
        let mut task = make_task("owner/repo#7", "Fix bug");
        task.ci = CiStatus::Failure; // " CI FAIL " status pill
        let ws = Workspace::from_task(task.clone(), fixed_time());
        let theme = theme();
        let ctx = ctx_for(&ws, &task, &theme);
        let columns = build_columns(2);
        let rows = vec![build_row(&ctx)];
        let lines = crate::components::table::render_table(&rows, &columns, 44);
        let line: String = lines[0].spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(line.contains('7'), "item number dropped: {line:?}");
        assert!(
            line.contains("Fix bug"),
            "short title should render in full: {line:?}",
        );
        assert!(
            line.contains("CI FAIL"),
            "status must survive when the short title leaves room: {line:?}",
        );
    }

    /// Regression for issue #130: at a tiny width even the role /
    /// state indicators shed, and the title finally elides with `…` —
    /// but the item number is still there, so the row is identifiable
    /// (the bug rendered `⬤ … C` with no number/title at all).
    #[test]
    fn tiny_width_preserves_number_and_elides_title_last() {
        let mut task = make_task("owner/repo#42", "Fix the broken sidebar layout");
        task.ci = CiStatus::Failure;
        let ws = Workspace::from_task(task.clone(), fixed_time());
        let theme = theme();
        let mut ctx = ctx_for(&ws, &task, &theme);
        ctx.asking = true; // state slot wants width too
        let columns = build_columns(2);
        let rows = vec![build_row(&ctx)];
        let lines = crate::components::table::render_table(&rows, &columns, 22);
        let line: String = lines[0].spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(
            line.contains("42"),
            "number must survive at tiny width: {line:?}"
        );
        assert!(
            line.contains("Fix the"),
            "title head must survive: {line:?}"
        );
        assert!(
            line.contains('…'),
            "title should elide at tiny width: {line:?}"
        );
        assert!(
            !line.contains("CI FAIL"),
            "status pill must be gone: {line:?}"
        );
        assert!(
            !line.contains('?'),
            "state slot must shed before the title: {line:?}"
        );
    }

    /// Labels render as bracketed chips with one leading space per
    /// chip. No labels → no spans, so a tag-less title cell is just
    /// the title with no reserved label width (#329).
    #[test]
    fn label_spans_empty_for_taskless_or_unlabeled_row() {
        let task = make_task("owner/repo#1", "x");
        let ws = Workspace::from_task(task.clone(), fixed_time());
        let theme = theme();
        let ctx = ctx_for(&ws, &task, &theme);
        assert!(label_spans(&ctx).is_empty());
        // Tag-less title cell: one span, no atomic tail.
        let title = cell_title(&ctx);
        assert_eq!(title.spans.len(), 1);
        assert_eq!(title.atomic_tail, 0);
    }

    #[test]
    fn label_spans_render_bracketed_chips() {
        let mut task = make_task("owner/repo#1", "x");
        task.labels = vec![
            lazybox_core::Label::new("bug"),
            lazybox_core::Label::new("ci"),
        ];
        let ws = Workspace::from_task(task.clone(), fixed_time());
        let theme = theme();
        let ctx = ctx_for(&ws, &task, &theme);
        let joined: String = label_spans(&ctx)
            .iter()
            .map(|s| s.content.as_ref())
            .collect();
        assert_eq!(joined, " [bug] [ci]");
        // They ride at the tail of the title cell, tagged as the atomic
        // drop unit — 2 chips × (space, `[`, name, `]`) = 8 spans.
        let title = cell_title(&ctx);
        let joined: String = title.spans.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(joined, "x [bug] [ci]");
        assert_eq!(title.atomic_tail, 8);
    }

    /// More than 3 labels collapses extras into a `+N` overflow
    /// indicator — the issue's "graceful truncation" requirement.
    #[test]
    fn label_spans_truncate_with_overflow_indicator() {
        let mut task = make_task("owner/repo#1", "x");
        task.labels = vec![
            lazybox_core::Label::new("bug"),
            lazybox_core::Label::new("ci"),
            lazybox_core::Label::new("backend"),
            lazybox_core::Label::new("priority"),
            lazybox_core::Label::new("docs"),
        ];
        let ws = Workspace::from_task(task.clone(), fixed_time());
        let theme = theme();
        let ctx = ctx_for(&ws, &task, &theme);
        let joined: String = label_spans(&ctx)
            .iter()
            .map(|s| s.content.as_ref())
            .collect();
        assert_eq!(joined, " [bug] [ci] [backend] +2");
    }

    /// A stale issue's fade now reaches its inline chips (#329): with
    /// the chips in the title cell, a full-color label beside a dimmed
    /// title would fight the "old, skip me" cue — so the visible chip
    /// spans dim too, but not on the cursor row.
    #[test]
    fn stale_issue_labels_are_dimmed_off_cursor() {
        let mut task = make_stale_task("owner/repo#1", "old issue");
        task.labels = vec![lazybox_core::Label::new("bug")];
        let ws = Workspace::from_task(task.clone(), fixed_time());
        let theme = theme();
        let mut ctx = ctx_for(&ws, &task, &theme);

        assert!(ctx.is_stale_issue());
        // The inter-chip separator keeps the plain row style (it's
        // invisible); every visible chip span carries DIM.
        assert!(
            label_spans(&ctx)
                .iter()
                .filter(|s| !s.content.trim().is_empty())
                .all(|s| s.style.add_modifier.contains(Modifier::DIM)),
            "stale issue's label chips should be dimmed",
        );

        ctx.is_cursor = true;
        assert!(
            label_spans(&ctx)
                .iter()
                .all(|s| !s.style.add_modifier.contains(Modifier::DIM)),
            "cursor row must not dim its label chips",
        );
    }

    /// A fresh (non-stale) labelled row keeps full-color chips.
    #[test]
    fn fresh_issue_labels_are_not_dimmed() {
        let mut task = make_task("owner/repo#1", "new issue");
        task.labels = vec![lazybox_core::Label::new("bug")];
        let ws = Workspace::from_task(task.clone(), fixed_time());
        let theme = theme();
        let ctx = ctx_for(&ws, &task, &theme);

        assert!(!ctx.is_stale_issue());
        assert!(
            label_spans(&ctx)
                .iter()
                .all(|s| !s.style.add_modifier.contains(Modifier::DIM)),
            "fresh issue's label chips should not be dimmed",
        );
    }

    /// Regression for issue #329: a tag-less row must NOT lose title
    /// width to another row's labels. Two rows go through one
    /// `render_table` call (as the sidebar does): one carries long
    /// `[dependencies] [go]` chips, the other has none. The label-less
    /// row's title has to render in full — the pre-fix global `Max`
    /// label column reserved the widest label cell on every row, so
    /// the tag-less title elided with `…` for no visible reason.
    #[test]
    fn tagless_title_not_truncated_by_another_rows_labels() {
        let theme = theme();
        let long = "Round-robin per-repo sync to reduce overhead";
        let mut labelled = make_task("owner/repo#1", "chore: bump deps");
        labelled.labels = vec![
            lazybox_core::Label::new("dependencies"),
            lazybox_core::Label::new("go"),
        ];
        let tagless = make_task("owner/repo#2", long);
        let ws_a = Workspace::from_task(labelled.clone(), fixed_time());
        let ws_b = Workspace::from_task(tagless.clone(), fixed_time());
        let ctx_a = ctx_for(&ws_a, &labelled, &theme);
        let ctx_b = ctx_for(&ws_b, &tagless, &theme);
        let columns = build_columns(4);
        let rows = vec![build_row(&ctx_a), build_row(&ctx_b)];
        let lines = crate::components::table::render_table(&rows, &columns, 62);
        let tagless_line: String = lines[1].spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(
            tagless_line.contains(long),
            "tag-less title truncated by another row's labels: {tagless_line:?}",
        );
        assert!(
            !tagless_line.contains('…'),
            "tag-less title should not elide: {tagless_line:?}",
        );
    }

    /// Issue #328, part 1: common verbose label names are aliased to a
    /// short form, and anything else past `MAX_CHIP_LEN` is capped with
    /// a trailing `…` so no one chip dominates the row.
    #[test]
    fn abbreviate_label_aliases_and_caps() {
        assert_eq!(abbreviate_label("dependencies"), "deps");
        assert_eq!(abbreviate_label("Dependencies"), "deps"); // case-insensitive
        assert_eq!(abbreviate_label("documentation"), "docs");
        assert_eq!(abbreviate_label("go"), "go"); // short, untouched
        // A long, non-aliased name caps at MAX_CHIP_LEN cells incl. `…`.
        let capped = abbreviate_label("github_actions");
        assert_eq!(capped, "github_ac…");
        assert_eq!(capped.chars().count(), MAX_CHIP_LEN);
    }

    /// Issue #328, part 1: the row from the screenshot — `[dependencies]
    /// [go]` — collapses to `[deps] [go]` so a Dependabot-heavy list
    /// stops eating the title's width.
    #[test]
    fn label_spans_abbreviate_long_names() {
        let mut task = make_task("owner/repo#1", "x");
        task.labels = vec![
            lazybox_core::Label::new("dependencies"),
            lazybox_core::Label::new("go"),
        ];
        let ws = Workspace::from_task(task.clone(), fixed_time());
        let theme = theme();
        let ctx = ctx_for(&ws, &task, &theme);
        let joined: String = label_spans(&ctx)
            .iter()
            .map(|s| s.content.as_ref())
            .collect();
        assert_eq!(joined, " [deps] [go]");
    }

    /// Issue #328, part 2: under width pressure the labels shed before
    /// the CONFLICT / CI status — the tags are the least important thing
    /// on the row, the merge-conflict signal is the most. A width that
    /// can't fit both keeps CONFLICT and drops the chips.
    #[test]
    fn narrow_width_sheds_labels_before_status() {
        let mut task = make_task("owner/repo#42", "Fix bug");
        task.mergeable = lazybox_core::Mergeable::Conflicting; // " CONFLICT "
        task.labels = vec![
            lazybox_core::Label::new("bug"),
            lazybox_core::Label::new("ci"),
        ];
        let ws = Workspace::from_task(task.clone(), fixed_time());
        let theme = theme();
        let ctx = ctx_for(&ws, &task, &theme);
        let columns = build_columns(2);
        let rows = vec![build_row(&ctx)];
        let lines = crate::components::table::render_table(&rows, &columns, 34);
        let line: String = lines[0].spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(
            line.contains("CONFLICT"),
            "the actionable status must survive: {line:?}",
        );
        assert!(
            !line.contains("[bug]") && !line.contains("[ci]"),
            "labels should shed before the status pill: {line:?}",
        );
        assert!(line.contains("Fix bug"), "title dropped: {line:?}");
    }

    /// Issue #328, part 3: the status pill is trimmed to its ` LABEL `
    /// block, so a single clean gap — the pill's own trailing space plus
    /// the time cell's leading space — separates the CI status from the
    /// duration, instead of the old baked-in trailing padding stacking
    /// with the column gap and the time's leading space.
    #[test]
    fn status_pill_sits_one_clean_gap_off_the_time() {
        let mut task = make_task("owner/repo#42", "Fix bug");
        task.ci = CiStatus::Failure;
        task.updated_at = fixed_time() - chrono::Duration::minutes(5); // "5m"
        let ws = Workspace::from_task(task.clone(), fixed_time());
        let theme = theme();
        let ctx = ctx_for(&ws, &task, &theme);
        let columns = build_columns(4);
        let rows = vec![build_row(&ctx)];
        let lines = crate::components::table::render_table(&rows, &columns, 80);
        let line: String = lines[0].spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(
            line.contains("CI FAIL  5m"),
            "expected a single clean gap between status and time: {line:?}",
        );
        assert!(
            !line.contains("CI FAIL   5m"),
            "status↔time gap is still oversized: {line:?}",
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

    /// Regression for issues #65 and #94: the type glyph must sit one
    /// space off the `NNN` on every row, regardless of how wide that
    /// row's number is. The bug was a right-aligned pr-number column:
    /// it padded shorter numbers on the LEFT, so `⇄ 312` rendered tight
    /// while `○ 7` picked up extra leading spaces (`○   7`) —
    /// inconsistent post-glyph spacing across rows. Left alignment moves
    /// the padding to the right, keeping the glyph→number gap a constant
    /// single space everywhere. (The `#` prefix itself was dropped in
    /// issue #67; the glyph now carries the issue-vs-PR signal.)
    #[test]
    fn type_glyph_has_single_space_before_number_for_mixed_widths() {
        let theme = theme();
        // Mixed glyph types AND mixed number widths (1/2/3 digits) so
        // `max_pr_num_width` is driven by `312` (3 cells). The narrower
        // rows are the ones the old right-align padded on the left; the
        // issue (`○`) row exercises the non-PR glyph path too, so a
        // regression can't hide on one glyph variant.
        let cases: [(Task, &str); 3] = [
            (make_task("owner/repo#7", "x"), "○ 7"), // issue, 1 digit
            (pr_task("owner/repo", 42), "⇄ 42"),     // PR, 2 digits
            (pr_task("owner/repo", 312), "⇄ 312"),   // PR, 3 digits
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
                "type glyph must sit one space off the number; \
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
            lazybox_core::WorkspaceKey("scratch-branch".into()),
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
            is_selected: false,
            max_pr_num_width: 4,
            asking: false,
            working: false,
            done: false,
            exited: false,
            working_glyph: working_glyph(0),
            badges: vec![],
            agent_number: None,
            ascii_glyphs: false,
            auto_merge_armed: false,
            auto_fix_armed: false,
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
