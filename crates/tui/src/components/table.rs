//! Minimal table layout for sidebar / right-pane row rendering.
//!
//! Pilot's "tabular" views (sidebar workspaces, activity cards) had
//! been hand-laying-out columns inline in their render functions:
//! ad-hoc padding, hardcoded widths, jitter when content widths
//! varied across rows (`#7204 R` vs `#31 R` putting the `R` at
//! different x positions). This module owns the column-width
//! arithmetic so renderers describe COLUMNS once and feed CELLS per
//! row — width is computed across all rows at the top of the pass.
//!
//! The public surface is intentionally small:
//!
//! - [`Column`] — one column's width strategy + minimum width.
//! - [`compute_widths`] — pure function: given a column spec and
//!   the natural width of each cell in each row, produce the final
//!   per-column width that respects `total_width`. Single source of
//!   truth for "which column eats the slack."
//!
//! Renderers stay in charge of producing `Span`s (theming + ratatui
//! style logic varies enough per surface that a single Cell type
//! would over-abstract). They call `compute_widths` once, then pad
//! each cell to the resulting width as they build their Line.

/// How wide a column should be.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColumnWidth {
    /// Always this many cells, regardless of content. Status pills
    /// and time columns use this — they have a stable max-width
    /// known up-front.
    Fixed(usize),
    /// At least `min` cells, expands to the widest cell across all
    /// rows. PR number column uses this: `#31` → 3 cells, `#7204` →
    /// 5 cells, all rows pad to whichever wins.
    Max { min: usize },
    /// Absorbs remaining horizontal space after Fixed + Max columns
    /// are subtracted from `total_width`. Title column uses this.
    /// If multiple Flex columns share a row, each gets an equal
    /// share. Truncates rather than overflowing.
    Flex { min: usize },
}

/// Horizontal alignment within a column. Drives whether the
/// padding-spaces sit to the *right* of the content (Left — default)
/// or to the *left* (Right). Sidebar's trailer columns (unread,
/// status pill, relative time) are Right; the title column is Left.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Align {
    #[default]
    Left,
    Right,
}

/// Default column priority — high, so a column is kept until every
/// lower-priority column has been dropped. Callers opt INTO being
/// shed first by lowering this with [`Column::priority`].
pub const DEFAULT_PRIORITY: u8 = u8::MAX;

/// One column's spec.
#[derive(Debug, Clone, Copy)]
pub struct Column {
    pub width: ColumnWidth,
    pub align: Align,
    /// Drop order when the row is too narrow to fit every column.
    /// Lower priority columns are dropped (set to width 0) FIRST so
    /// the high-priority ones keep their space. Flex columns are never
    /// dropped — they shrink to their `min` and then elide with `…`.
    /// Defaults to [`DEFAULT_PRIORITY`] (kept until last).
    pub priority: u8,
}

impl Column {
    pub fn fixed(width: usize) -> Self {
        Self {
            width: ColumnWidth::Fixed(width),
            align: Align::Left,
            priority: DEFAULT_PRIORITY,
        }
    }

    pub fn max(min: usize) -> Self {
        Self {
            width: ColumnWidth::Max { min },
            align: Align::Left,
            priority: DEFAULT_PRIORITY,
        }
    }

    pub fn flex(min: usize) -> Self {
        Self {
            width: ColumnWidth::Flex { min },
            align: Align::Left,
            priority: DEFAULT_PRIORITY,
        }
    }

    /// Right-align the column. Content sits flush to the right edge;
    /// padding spaces fill the left side. Chainable: `Column::fixed(4).right()`.
    pub fn right(mut self) -> Self {
        self.align = Align::Right;
        self
    }

    /// Set the drop priority — lower is dropped first when the table is
    /// too narrow to fit every column. Chainable: `Column::max(0).priority(10)`.
    pub fn priority(mut self, priority: u8) -> Self {
        self.priority = priority;
        self
    }
}

/// Compute the final per-column width given the column specs, the
/// natural per-cell content widths (rows × cols), and the total
/// available row width.
///
/// Algorithm:
///   1. Fixed columns take their declared width.
///   2. Max columns take `max(min, max-across-rows-of-cell-width)`.
///   3. Flex columns claim their `min` up front.
///   4. If the natural widths overflow `total_width`, shed the
///      lowest-[`priority`](Column::priority) non-flex columns (set
///      them to 0) one at a time until the row fits or there's
///      nothing droppable left. This is what makes a narrow sidebar
///      degrade gracefully — secondary columns (time, status,
///      indicators) drop out before the title is squeezed to a lone
///      `…`. Flex columns are never dropped here.
///   5. Whatever space is left over goes to the Flex columns, split
///      equally. If even the kept columns + flex mins still overflow
///      (extremely narrow pane), the flex columns shrink below their
///      min — down to 0 — and the renderer elides their content.
///
/// `cell_widths[row][col]` is the natural width of the cell's
/// content in cells. Empty for an empty table.
pub fn compute_widths(
    columns: &[Column],
    cell_widths: &[Vec<usize>],
    total_width: usize,
) -> Vec<usize> {
    let n = columns.len();
    let mut widths = vec![0usize; n];

    let mut consumed = 0usize;
    let mut flex_indices: Vec<usize> = Vec::new();

    for (i, col) in columns.iter().enumerate() {
        match col.width {
            ColumnWidth::Fixed(w) => {
                widths[i] = w;
                consumed = consumed.saturating_add(w);
            }
            ColumnWidth::Max { min } => {
                let max_observed = cell_widths
                    .iter()
                    .map(|row| row.get(i).copied().unwrap_or(0))
                    .max()
                    .unwrap_or(0);
                widths[i] = max_observed.max(min);
                consumed = consumed.saturating_add(widths[i]);
            }
            ColumnWidth::Flex { min } => {
                flex_indices.push(i);
                widths[i] = min;
                consumed = consumed.saturating_add(min);
            }
        }
    }

    // Too narrow: shed the lowest-priority non-flex columns (widest
    // would be cheapest, but priority order is what the caller asked
    // for) until the row fits or nothing droppable remains. Dropping
    // a whole column rather than half-rendering it keeps chip-like
    // content (status pills, badges) legible — a clipped `REVIE…`
    // pill carries no signal.
    if consumed > total_width {
        let mut droppable: Vec<usize> = (0..n)
            .filter(|&i| {
                !flex_indices.contains(&i)
                    && widths[i] > 0
                    && columns[i].priority < DEFAULT_PRIORITY
            })
            .collect();
        droppable.sort_by_key(|&i| columns[i].priority);
        for i in droppable {
            if consumed <= total_width {
                break;
            }
            consumed -= widths[i];
            widths[i] = 0;
        }
    }

    if flex_indices.is_empty() {
        return widths;
    }

    if consumed <= total_width {
        // Hand the slack to the flex columns, split equally.
        let remaining = total_width - consumed;
        let per_flex = remaining / flex_indices.len();
        let leftover = remaining % flex_indices.len();
        for (idx, &col_idx) in flex_indices.iter().enumerate() {
            widths[col_idx] += per_flex;
            if idx < leftover {
                widths[col_idx] += 1;
            }
        }
    } else {
        // Even the kept columns + flex mins overflow. Reclaim the
        // excess from the flex columns themselves (down to 0); the
        // renderer truncates the remaining title with `…`.
        let mut excess = consumed - total_width;
        for &col_idx in &flex_indices {
            let reduce = widths[col_idx].min(excess);
            widths[col_idx] -= reduce;
            excess -= reduce;
            if excess == 0 {
                break;
            }
        }
    }
    widths
}

/// One cell — a sequence of pre-styled spans the caller assembled.
/// `width()` is the visible cell count; `render_table` uses it to
/// know how much padding to add or where to truncate.
///
/// `fill_style`, when set, styles the padding spaces this cell's
/// renderer emits. The cursor-highlight row uses it so the row's
/// background colour extends across every cell — without it, gaps
/// between content and the next cell render as bare spaces and the
/// highlight looks broken.
#[derive(Debug, Clone, Default)]
pub struct Cell {
    pub spans: Vec<ratatui::text::Span<'static>>,
    pub fill_style: Option<ratatui::style::Style>,
}

impl Cell {
    pub fn new(spans: Vec<ratatui::text::Span<'static>>) -> Self {
        Self {
            spans,
            fill_style: None,
        }
    }

    pub fn from_span(span: ratatui::text::Span<'static>) -> Self {
        Self {
            spans: vec![span],
            fill_style: None,
        }
    }

    pub fn empty() -> Self {
        Self::default()
    }

    /// Set the padding-fill style for this cell only. Chainable.
    pub fn fill(mut self, style: ratatui::style::Style) -> Self {
        self.fill_style = Some(style);
        self
    }

    /// Total visible width of the cell — sum of all span widths in
    /// display cells (not bytes). Uses unicode-width via
    /// `crate::util::visual_width` so multi-byte glyphs count once.
    pub fn width(&self) -> usize {
        self.spans
            .iter()
            .map(|s| crate::util::visual_width(s.content.as_ref()))
            .sum()
    }
}

/// One row's cells. The slice order matches the `Column` slice
/// passed to `render_table`. Rows can have fewer cells than columns
/// (missing cells render as empty padded space).
///
/// `fill_style` is the row-level default for cell padding (any
/// `Cell::fill_style` overrides per-cell). The sidebar's cursor row
/// uses it so every column's padding inherits the highlight bg —
/// callers don't have to remember to set it on each cell.
#[derive(Debug, Clone, Default)]
pub struct Row {
    pub cells: Vec<Cell>,
    pub fill_style: Option<ratatui::style::Style>,
}

impl Row {
    pub fn new(cells: Vec<Cell>) -> Self {
        Self {
            cells,
            fill_style: None,
        }
    }

    /// Set a row-level fill style that every cell's padding inherits
    /// (unless the cell has its own `fill_style`). Chainable.
    pub fn fill(mut self, style: ratatui::style::Style) -> Self {
        self.fill_style = Some(style);
        self
    }
}

/// Clip `line`'s visible width to `max_cells`, preserving span
/// styles, with a final `…` when truncation actually happened.
///
/// Necessary because ratatui's `Paragraph` silently CLIPS content
/// past `area.width` — there's no built-in "truncate-with-ellipsis"
/// affordance. A user-visible empty-state hint like
/// `"(no terminals — press s for shell, c for claude)"` rendered
/// into a narrow right pane just stopped mid-word, with no visual
/// indicator that content was missing. Pre-truncating with `…`
/// makes the cut obvious.
///
/// Returns `line` unchanged when it already fits — cheap fast-path
/// for the common "fits comfortably" case.
pub fn truncate_line<'a>(
    line: ratatui::text::Line<'a>,
    max_cells: usize,
) -> ratatui::text::Line<'a> {
    let total: usize = line
        .spans
        .iter()
        .map(|s| crate::util::visual_width(s.content.as_ref()))
        .sum();
    if total <= max_cells {
        return line;
    }
    if max_cells == 0 {
        return ratatui::text::Line::from(Vec::<ratatui::text::Span>::new());
    }
    // Reserve a cell for the trailing `…`. We always emit the
    // ellipsis when we truncated, so callers can rely on the cut
    // being visible.
    let budget = max_cells - 1;
    let mut out: Vec<ratatui::text::Span<'a>> = Vec::with_capacity(line.spans.len() + 1);
    let mut consumed = 0usize;
    for span in line.spans.into_iter() {
        let w = crate::util::visual_width(span.content.as_ref());
        if consumed + w <= budget {
            consumed += w;
            out.push(span);
            continue;
        }
        let remaining = budget - consumed;
        if remaining > 0 {
            let truncated: String = span
                .content
                .chars()
                .scan(0usize, |used, ch| {
                    let cw = crate::util::char_visual_width(ch);
                    if *used + cw > remaining {
                        return None;
                    }
                    *used += cw;
                    Some(ch)
                })
                .collect();
            out.push(ratatui::text::Span::styled(truncated, span.style));
        }
        break;
    }
    out.push(ratatui::text::Span::raw("…"));
    ratatui::text::Line::from(out)
}

/// Render a table of rows to ratatui Lines, with each cell padded
/// (right) to its computed column width. Cells wider than their
/// column get their last span truncated with `…`. This is the
/// renderer half of the abstraction — `compute_widths` does the
/// math, this turns rows into Lines. Sidebar / right_pane consume
/// it instead of hand-stitching span sequences with hardcoded
/// padding.
pub fn render_table(
    rows: &[Row],
    columns: &[Column],
    total_width: usize,
) -> Vec<ratatui::text::Line<'static>> {
    let cell_widths: Vec<Vec<usize>> = rows
        .iter()
        .map(|r| r.cells.iter().map(|c| c.width()).collect())
        .collect();
    let widths = compute_widths(columns, &cell_widths, total_width);

    rows.iter()
        .map(|row| render_row(row, columns, &widths))
        .collect()
}

fn render_row(row: &Row, columns: &[Column], widths: &[usize]) -> ratatui::text::Line<'static> {
    let mut spans: Vec<ratatui::text::Span<'static>> = Vec::new();
    for (i, target_w) in widths.iter().enumerate() {
        // A zero-width column emits NOTHING — not even a `…`. This
        // matches the prior hand-rolled sidebar behavior where
        // `saturating_sub` clamped the title budget to 0 and the
        // title text was simply omitted on cramped panes (rather
        // than displaying a lone ellipsis where the title should
        // be).
        if *target_w == 0 {
            continue;
        }
        let empty = Cell::empty();
        let cell = row.cells.get(i).unwrap_or(&empty);
        let align = columns.get(i).map(|c| c.align).unwrap_or(Align::Left);
        // Fill resolution: cell override > row default > unstyled.
        let fill_style = cell.fill_style.or(row.fill_style).unwrap_or_default();
        let cell_w = cell.width();
        if cell_w <= *target_w {
            let pad = *target_w - cell_w;
            let pad_span = ratatui::text::Span::styled(" ".repeat(pad), fill_style);
            match align {
                Align::Left => {
                    spans.extend(cell.spans.iter().cloned());
                    if pad > 0 {
                        spans.push(pad_span);
                    }
                }
                Align::Right => {
                    if pad > 0 {
                        spans.push(pad_span);
                    }
                    spans.extend(cell.spans.iter().cloned());
                }
            }
        } else {
            // Truncate: walk spans until we've consumed `target_w - 1`
            // cells, then push a `…` to mark the cut. Truncation
            // always clips on the right edge regardless of align —
            // a right-aligned over-wide cell is an unusual case and
            // clipping the left would lose the high-signal end (e.g.
            // a status pill's label).
            let mut consumed = 0usize;
            let budget = target_w.saturating_sub(1);
            for span in &cell.spans {
                let span_w = crate::util::visual_width(span.content.as_ref());
                if consumed + span_w <= budget {
                    spans.push(span.clone());
                    consumed += span_w;
                } else {
                    let remaining = budget - consumed;
                    if remaining > 0 {
                        let truncated: String = span
                            .content
                            .chars()
                            .scan(0usize, |w, ch| {
                                let cw = crate::util::char_visual_width(ch);
                                if *w + cw > remaining {
                                    return None;
                                }
                                *w += cw;
                                Some(ch)
                            })
                            .collect();
                        spans.push(ratatui::text::Span::styled(truncated, span.style));
                    }
                    break;
                }
            }
            spans.push(ratatui::text::Span::raw("…"));
        }
    }
    ratatui::text::Line::from(spans)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::text::Span;

    #[test]
    fn fixed_takes_declared_width() {
        let cols = [Column::fixed(5), Column::fixed(10)];
        let widths = compute_widths(&cols, &[], 100);
        assert_eq!(widths, vec![5, 10]);
    }

    #[test]
    fn max_takes_widest_observed_clamped_to_min() {
        let cols = [Column::max(3)];
        // Rows have natural widths 1, 4, 2 in col 0.
        let widths = compute_widths(&cols, &[vec![1], vec![4], vec![2]], 100);
        // max(3, 4) = 4.
        assert_eq!(widths, vec![4]);
    }

    #[test]
    fn max_uses_min_when_all_rows_narrower() {
        let cols = [Column::max(5)];
        let widths = compute_widths(&cols, &[vec![1], vec![2]], 100);
        assert_eq!(widths, vec![5]);
    }

    #[test]
    fn flex_absorbs_remaining_space() {
        // 100 total, fixed 10 + max 5 + flex = 100 → flex = 85.
        let cols = [Column::fixed(10), Column::max(5), Column::flex(1)];
        let widths = compute_widths(&cols, &[vec![0, 3, 0]], 100);
        assert_eq!(widths, vec![10, 5, 85]);
    }

    #[test]
    fn multiple_flex_columns_split_evenly_with_remainder_on_left() {
        // 100 - 10 = 90 remaining across 3 flexes → 30 each, no remainder.
        let cols = [
            Column::fixed(10),
            Column::flex(1),
            Column::flex(1),
            Column::flex(1),
        ];
        let widths = compute_widths(&cols, &[], 100);
        assert_eq!(widths, vec![10, 30, 30, 30]);
    }

    #[test]
    fn multiple_flex_columns_distribute_remainder_left_to_right() {
        // 11 remaining across 3 flexes → 3 + 3 + 3 + 2 leftover →
        // leftover spreads to the leftmost flex columns.
        let cols = [Column::flex(1), Column::flex(1), Column::flex(1)];
        let widths = compute_widths(&cols, &[], 11);
        // per_flex = 3, leftover = 2 → cols 0,1 get +1.
        assert_eq!(widths, vec![4, 4, 3]);
    }

    #[test]
    fn flex_shrinks_below_min_when_no_droppable_columns_fit() {
        // Fixed 60 + flex_min 50 = 110, total = 100. The fixed column
        // has DEFAULT_PRIORITY (not droppable), so the flex column
        // absorbs the 10-cell overflow and shrinks to 40 — the
        // renderer then elides its content with `…` instead of letting
        // the row overflow and clip without warning.
        let cols = [Column::fixed(60), Column::flex(50)];
        let widths = compute_widths(&cols, &[], 100);
        assert_eq!(widths, vec![60, 40]);
    }

    #[test]
    fn low_priority_columns_drop_before_flex_shrinks() {
        // Fixed 10 (kept) + max 20 (low priority) + flex_min 30 = 60,
        // total = 40. The max column is dropped (priority < default),
        // freeing its 20 cells; the flex then keeps its full min.
        let cols = [
            Column::fixed(10),
            Column::max(20).priority(10),
            Column::flex(30),
        ];
        let widths = compute_widths(&cols, &[vec![0, 20, 0]], 40);
        assert_eq!(widths, vec![10, 0, 30]);
    }

    #[test]
    fn columns_drop_in_ascending_priority_order() {
        // Two droppable max columns; only enough overflow to need one.
        // The lower-priority one (5) goes first; the higher (50) stays.
        let cols = [
            Column::fixed(10),
            Column::max(8).priority(50),
            Column::max(8).priority(5),
            Column::flex(10),
        ];
        // 10 + 8 + 8 + 10 = 36, total = 30 → must shed 6+. Dropping the
        // priority-5 column frees 8 → fits. priority-50 column survives.
        let widths = compute_widths(&cols, &[vec![0, 8, 8, 0]], 30);
        assert_eq!(widths[2], 0, "lowest-priority column should drop first");
        assert_eq!(widths[1], 8, "higher-priority column should survive");
        // Flex reclaims the slack freed by the drop.
        assert_eq!(widths[3], 30 - 10 - 8);
    }

    #[test]
    fn empty_table_returns_min_or_declared() {
        let cols = [Column::fixed(5), Column::max(3), Column::flex(1)];
        let widths = compute_widths(&cols, &[], 20);
        // fixed=5, max=3 (no rows, uses min), flex=remaining=12.
        assert_eq!(widths, vec![5, 3, 12]);
    }

    /// Cell width sums its spans by visual cells, not bytes.
    #[test]
    fn cell_width_sums_span_widths() {
        let cell = Cell::new(vec![Span::raw("hi"), Span::raw(" ●")]);
        assert_eq!(cell.width(), 4);
    }

    /// Render pads short cells out to column width.
    #[test]
    fn render_pads_short_cells_to_column_width() {
        let cols = [Column::fixed(5), Column::fixed(3)];
        let rows = vec![Row::new(vec![
            Cell::from_span(Span::raw("a")),
            Cell::from_span(Span::raw("b")),
        ])];
        let lines = render_table(&rows, &cols, 20);
        // "a" + 4 spaces + "b" + 2 spaces = 8 cells across the two columns.
        let joined: String = lines[0].spans.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(joined, "a    b  ");
    }

    /// Render truncates over-wide cells with an ellipsis.
    #[test]
    fn render_truncates_overlong_cell_with_ellipsis() {
        let cols = [Column::fixed(5)];
        let rows = vec![Row::new(vec![Cell::from_span(Span::raw("hello world"))])];
        let lines = render_table(&rows, &cols, 5);
        let joined: String = lines[0].spans.iter().map(|s| s.content.as_ref()).collect();
        // "hell" + "…" = 5 cells.
        assert_eq!(joined, "hell…");
    }

    /// Right-aligned column puts padding on the LEFT.
    #[test]
    fn right_align_pads_on_left() {
        let cols = [Column::fixed(5).right()];
        let rows = vec![Row::new(vec![Cell::from_span(Span::raw("hi"))])];
        let lines = render_table(&rows, &cols, 5);
        let joined: String = lines[0].spans.iter().map(|s| s.content.as_ref()).collect();
        // 3 spaces of padding, then "hi" flush right.
        assert_eq!(joined, "   hi");
    }

    /// Cell fill_style styles the padding spans (not the content).
    /// This is what extends the cursor highlight bg across a row.
    #[test]
    fn cell_fill_style_applies_to_padding_spans() {
        use ratatui::style::{Color, Style};
        let highlight = Style::default().bg(Color::Blue);
        let cols = [Column::fixed(5)];
        let rows = vec![Row::new(vec![
            Cell::from_span(Span::raw("a")).fill(highlight),
        ])];
        let lines = render_table(&rows, &cols, 5);
        // 2 spans: content (no fill) + padding (highlight bg).
        assert_eq!(lines[0].spans.len(), 2);
        assert_eq!(lines[0].spans[0].style, Style::default());
        assert_eq!(lines[0].spans[1].style, highlight);
        assert_eq!(lines[0].spans[1].content.as_ref(), "    ");
    }

    /// Row-level fill_style applies to every cell that hasn't
    /// overridden it. This is the cursor-row entry point.
    #[test]
    fn row_fill_style_applies_to_every_cell_padding() {
        use ratatui::style::{Color, Style};
        let highlight = Style::default().bg(Color::Blue);
        let cols = [Column::fixed(3), Column::fixed(3)];
        let rows = vec![
            Row::new(vec![
                Cell::from_span(Span::raw("a")),
                Cell::from_span(Span::raw("b")),
            ])
            .fill(highlight),
        ];
        let lines = render_table(&rows, &cols, 6);
        // Both padding spans inherit the row's fill_style.
        let padding_spans: Vec<&ratatui::text::Span> = lines[0]
            .spans
            .iter()
            .filter(|s| s.content.trim().is_empty() && !s.content.is_empty())
            .collect();
        assert_eq!(padding_spans.len(), 2);
        for s in padding_spans {
            assert_eq!(s.style, highlight);
        }
    }

    /// Cell fill_style wins over the row default.
    #[test]
    fn cell_fill_overrides_row_fill() {
        use ratatui::style::{Color, Style};
        let row_fill = Style::default().bg(Color::Blue);
        let cell_fill = Style::default().bg(Color::Red);
        let cols = [Column::fixed(3)];
        let rows =
            vec![Row::new(vec![Cell::from_span(Span::raw("a")).fill(cell_fill)]).fill(row_fill)];
        let lines = render_table(&rows, &cols, 3);
        // Padding span uses the cell-level fill, not the row one.
        assert_eq!(lines[0].spans[1].style, cell_fill);
    }

    /// Max column picks the widest cell across all rows.
    #[test]
    fn render_uses_widest_observed_for_max_column() {
        let cols = [Column::max(3)];
        let rows = vec![
            Row::new(vec![Cell::from_span(Span::raw("#1"))]),
            Row::new(vec![Cell::from_span(Span::raw("#7204"))]),
        ];
        let lines = render_table(&rows, &cols, 20);
        // Both rows pad to 5 cells (width of "#7204").
        let row0: String = lines[0].spans.iter().map(|s| s.content.as_ref()).collect();
        let row1: String = lines[1].spans.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(row0, "#1   ");
        assert_eq!(row1, "#7204");
    }

    /// Short line fits unchanged — fast path.
    #[test]
    fn truncate_line_fits_unchanged() {
        let line = ratatui::text::Line::from(vec![Span::raw("hello "), Span::raw("world")]);
        let out = truncate_line(line, 20);
        let s: String = out.spans.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(s, "hello world");
    }

    /// Line longer than budget gets a trailing `…`. Spans split at
    /// the truncation point keep their styles.
    #[test]
    fn truncate_line_clips_with_ellipsis_at_span_boundary() {
        let style = ratatui::style::Style::default().fg(ratatui::style::Color::Red);
        let line =
            ratatui::text::Line::from(vec![Span::raw("hello "), Span::styled("world", style)]);
        let out = truncate_line(line, 8);
        let s: String = out.spans.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(s, "hello w…");
        // Second-span style preserved on the truncated remainder.
        assert_eq!(out.spans[1].style, style);
    }

    /// User-visible bug regression: the right-pane terminal-pane
    /// empty hint was clipping mid-word with no `…`. Pin the
    /// truncated form so we don't drift back.
    #[test]
    fn truncate_line_handles_user_visible_no_terminals_hint() {
        let hint = "(no terminals — press s for shell, c for claude)";
        let line = ratatui::text::Line::from(Span::raw(hint));
        let out = truncate_line(line, 30);
        let s: String = out.spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(s.ends_with('…'), "expected `…` suffix, got: {s:?}");
        assert!(crate::util::visual_width(&s) <= 30);
    }

    #[test]
    fn truncate_line_budget_zero_returns_empty() {
        let line = ratatui::text::Line::from(Span::raw("anything"));
        let out = truncate_line(line, 0);
        assert!(out.spans.is_empty());
    }
}
