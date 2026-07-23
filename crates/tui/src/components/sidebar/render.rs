//! The sidebar's `render` method. Pulled into its own file because
//! it's 280 lines on its own — the V1-style header strip + the
//! visible-rows render loop + the click-hit-test population each
//! have non-trivial layout logic, and inlining them next to the
//! key-handler made the parent `impl` block hard to navigate.

use super::*;
use crate::components::table::Row as TableRow;
use crate::components::workspace_row::{WorkspaceRowCtx, build_row as build_workspace_row};

fn spans_visual_width(spans: &[Span<'_>]) -> usize {
    spans
        .iter()
        .map(|span| visual_width(span.content.as_ref()))
        .sum()
}

/// Extend a cursor row's highlight to the right edge: pad with blank
/// cells in the row-background `style` so the selection fill spans the
/// full `row_budget` instead of hugging the text. Width is measured
/// from the spans themselves, so the fill stays correct regardless of
/// what glyphs the row carries. Caller guards on the row being the
/// cursor; non-cursor rows have no background to extend.
fn extend_cursor_fill(spans: &mut Vec<Span<'_>>, row_budget: usize, style: Style) {
    let used = spans_visual_width(spans);
    if used < row_budget {
        spans.push(Span::styled(" ".repeat(row_budget - used), style));
    }
}

impl Sidebar {
    pub fn render(&mut self, area: Rect, frame: &mut Frame, focused: bool) {
        // V1-style header strip:
        //   row 0: LAZYBOX vX.Y.Z  ● N new  ? N input        N items  7d
        //   row 1: s  filter (needs:reply ci:failed ...)
        //   row 2: N CI  N review               (omitted when both 0)
        //   row 3: ── divider ────────────────
        //   row 4: blank
        //   row 5+: content
        let theme = crate::theme::current();
        let now = self.now();
        let mailbox_label = match self.mailbox {
            Mailbox::Inbox => "LAZYBOX",
            Mailbox::Inactive => "INACTIVE",
            Mailbox::Snoozed => "SNOOZED",
        };
        let count = self.workspace_count();
        let unread = self.total_unread_count();
        let input_pending = self.input_pending_count();
        let ci_failing = self.ci_failing_count();
        let review_pending = self.review_pending_count();

        // Right inset reserves only the scroll-indicator column (drawn
        // at `area.width - 1`); there's no extra edge margin stacked on
        // top of it, so the scrollbar is the sole right-edge gutter and
        // content runs right up to it (issue #231).
        let l_pad: u16 = 1;
        let r_pad: u16 = 1;
        let inner_width = area.width.saturating_sub(l_pad + r_pad);

        // Row 0 — brand/mailbox on the left, attention badges in the
        // middle, item/window summary right-aligned when there is room.
        let mut header_left: Vec<Span> = Vec::with_capacity(4);
        header_left.push(Span::styled(mailbox_label, theme.title(focused)));
        // Brand-tied build version, so a running instance is identifiable
        // at a glance (e.g. confirming a fix actually shipped). Only on
        // the Inbox view, where the title is the app name rather than a
        // mailbox label.
        if matches!(self.mailbox, Mailbox::Inbox) {
            header_left.push(Span::raw(" "));
            header_left.push(Span::styled(
                concat!("v", env!("CARGO_PKG_VERSION")),
                Style::default().fg(theme.text_dim),
            ));
            // Neutral build-provenance tag on dev/source builds: it marks
            // the binary as one rebuilt from its source checkout rather
            // than replaced by an installer.
            if !crate::build_guard::is_release_build() {
                header_left.push(Span::styled(" (dev)", Style::default().fg(theme.text_dim)));
            }
        }
        let mut signal_spans: Vec<Span> = Vec::with_capacity(8);
        if unread > 0 {
            signal_spans.push(Span::styled(
                "● ",
                Style::default()
                    .fg(theme.hover)
                    .add_modifier(Modifier::BOLD),
            ));
            signal_spans.push(Span::styled(
                format!("{unread} new"),
                Style::default()
                    .fg(theme.hover)
                    .add_modifier(Modifier::BOLD),
            ));
        }
        if input_pending > 0 {
            if !signal_spans.is_empty() {
                signal_spans.push(Span::raw("  "));
            }
            signal_spans.push(Span::styled(
                "? ",
                Style::default().fg(theme.warn).add_modifier(Modifier::BOLD),
            ));
            signal_spans.push(Span::styled(
                format!("{input_pending} input"),
                Style::default().fg(theme.warn).add_modifier(Modifier::BOLD),
            ));
        }
        // Sleep-inhibition badge: painted exactly while the daemon's
        // keep-awake watcher holds its assertion (`ui.keep_awake` on
        // and ≥1 agent working), so the user can tell at a glance why
        // the machine isn't sleeping.
        // `☼` (U+263C) rather than an emoji: like the header's `●`
        // it's an ambiguous-width BMP symbol every terminal font
        // renders one cell wide, so the right-aligned summary can't
        // drift on fonts that draw emoji narrow.
        if self.keep_awake && self.any_agent_working() {
            if !signal_spans.is_empty() {
                signal_spans.push(Span::raw("  "));
            }
            signal_spans.push(Span::styled("☼ awake", Style::default().fg(theme.text_dim)));
        }

        let mut summary_spans: Vec<Span> = Vec::with_capacity(4);
        summary_spans.push(Span::styled(
            count.to_string(),
            Style::default().fg(theme.warn).add_modifier(Modifier::BOLD),
        ));
        if inner_width >= 30 {
            summary_spans.push(Span::styled(
                if count == 1 { " item" } else { " items" },
                Style::default().fg(theme.text_dim),
            ));
        }
        summary_spans.push(Span::styled("  7d", Style::default().fg(theme.text_dim)));

        let mut header_spans = header_left;
        let summary_width = spans_visual_width(&summary_spans);
        let signal_width = spans_visual_width(&signal_spans);
        let current_width = spans_visual_width(&header_spans);
        if !signal_spans.is_empty()
            && inner_width as usize >= current_width + 2 + signal_width + 2 + summary_width
        {
            header_spans.push(Span::raw("  "));
            header_spans.extend(signal_spans);
        }
        let current_width = spans_visual_width(&header_spans);
        if inner_width as usize > current_width + summary_width {
            let gap = inner_width as usize - current_width - summary_width;
            header_spans.push(Span::raw(" ".repeat(gap)));
            header_spans.extend(summary_spans);
        } else if inner_width as usize >= current_width + 2 {
            header_spans.push(Span::raw("  "));
            header_spans.push(Span::styled("[7d]", Style::default().fg(theme.text_dim)));
        }

        let row0 = Rect::new(area.x + l_pad, area.y, inner_width, 1.min(area.height));
        frame.render_widget(Paragraph::new(Line::from(header_spans)), row0);

        // Row 1 — filter + sort chips, both clickable. Each chip is
        // dim while at its default, accent-bold when the user has
        // selected a non-default value, so the row stays quiet by
        // default but visually shouts when a filter is on. The filter
        // chip lists every active filter (comma-joined) — clicking it
        // opens the filter menu.
        if area.height >= 2 {
            let row1 = Rect::new(area.x + l_pad, area.y + 1, inner_width, 1);

            let filter_active = !self.filters.is_empty();
            let sort_active = self.sort_mode != SortMode::Recent;
            let active_style = Style::default()
                .fg(theme.accent)
                .add_modifier(Modifier::BOLD);
            let dim_style = Style::default().fg(theme.text_dim);
            let key_style = active_style;

            let filter_prefix = "f ";
            // Collapse past two chips to `a, b, +N` so a wide active
            // set can't push the sort chip off the row or truncate a
            // label mid-word — the full set is one `f` away.
            let filter_chip = if filter_active {
                let chips = self.filters.chips();
                let shown = if chips.len() <= 2 {
                    chips.join(", ")
                } else {
                    format!("{}, +{}", chips[..2].join(", "), chips.len() - 2)
                };
                format!("[{shown}]")
            } else {
                "[filter]".to_string()
            };
            let sep = "  ";
            let sort_prefix = "o ";
            let sort_chip = format!("[{}]", self.sort_mode.chip_label());

            let filter_prefix_cells = visual_width(filter_prefix) as u16;
            let filter_chip_cells = visual_width(&filter_chip) as u16;
            let sep_cells = visual_width(sep) as u16;
            let sort_prefix_cells = visual_width(sort_prefix) as u16;
            let sort_chip_cells = visual_width(&sort_chip) as u16;

            frame.render_widget(
                Paragraph::new(Line::from(vec![
                    Span::styled(filter_prefix, key_style),
                    Span::styled(
                        filter_chip.clone(),
                        if filter_active {
                            active_style
                        } else {
                            dim_style
                        },
                    ),
                    Span::raw(sep),
                    Span::styled(sort_prefix, key_style),
                    Span::styled(
                        sort_chip.clone(),
                        if sort_active { active_style } else { dim_style },
                    ),
                ])),
                row1,
            );

            // Record clickable rects. Filter zone = `f ` + chip;
            // sort zone = `o ` + chip. The intervening separator is
            // a dead zone — clicks there are ignored.
            let filter_w = (filter_prefix_cells + filter_chip_cells).min(row1.width);
            self.filter_chip_rect = Some(Rect {
                x: row1.x,
                y: row1.y,
                width: filter_w,
                height: 1,
            });
            let sort_x = row1.x + filter_w + sep_cells;
            let remaining = row1.width.saturating_sub(filter_w + sep_cells);
            let sort_w = (sort_prefix_cells + sort_chip_cells).min(remaining);
            self.sort_chip_rect = if sort_w > 0 {
                Some(Rect {
                    x: sort_x,
                    y: row1.y,
                    width: sort_w,
                    height: 1,
                })
            } else {
                None
            };
        } else {
            self.filter_chip_rect = None;
            self.sort_chip_rect = None;
        }

        // Row 2 — stats summary, only when there's something to summarize.
        let mut stats_spans: Vec<Span> = Vec::new();
        if ci_failing > 0 {
            stats_spans.push(Span::styled(
                ci_failing.to_string(),
                Style::default()
                    .fg(theme.error)
                    .add_modifier(Modifier::BOLD),
            ));
            stats_spans.push(Span::styled(" CI", Style::default().fg(theme.text_dim)));
        }
        if review_pending > 0 {
            if !stats_spans.is_empty() {
                stats_spans.push(Span::raw("  "));
            }
            stats_spans.push(Span::styled(
                review_pending.to_string(),
                Style::default()
                    .fg(theme.accent)
                    .add_modifier(Modifier::BOLD),
            ));
            stats_spans.push(Span::styled(" review", Style::default().fg(theme.text_dim)));
        }
        if !stats_spans.is_empty() && area.height >= 3 {
            let row2 = Rect::new(area.x + l_pad, area.y + 2, inner_width, 1);
            frame.render_widget(Paragraph::new(Line::from(stats_spans)), row2);
        }

        // Row 3 — thin divider; accent-tinted while this pane has
        // focus so the active pane reads at a glance (#286).
        if area.height >= 4 {
            let div_area = Rect::new(area.x + l_pad, area.y + 3, inner_width, 1);
            let divider = "─".repeat(div_area.width as usize);
            frame.render_widget(
                Paragraph::new(Line::from(Span::styled(
                    divider,
                    theme.pane_divider(focused),
                ))),
                div_area,
            );
        }

        // Content starts at row 5 (skipping a blank row for breathing
        // room above the first item). When the `/` search bar is open
        // it claims the bottom row, so the list loses one line — the
        // bar is pinned to the bottom (fzf-style) so the repo tree
        // doesn't shift as the user types.
        const HEADER_HEIGHT: u16 = 5;
        let search_bar = self.search.is_some() && area.height > HEADER_HEIGHT;
        let inner = Rect {
            x: area.x + l_pad,
            y: area.y + HEADER_HEIGHT,
            width: inner_width,
            height: area
                .height
                .saturating_sub(HEADER_HEIGHT + u16::from(search_bar)),
        };

        let row_budget = inner_width as usize;
        // Pre-pass: compute the widest `NNN` across visible workspace
        // rows so every row pads to the same column. Without this,
        // `#7204 R` and `#31 R` had different role-letter positions
        // and the whole column visibly jittered.
        //
        // Width is the digit count of `n` (no `#` prefix — issue #67;
        // the type glyph in column 1 now carries the issue-vs-PR
        // signal), computed via `ilog10` so the hot path doesn't
        // allocate a String per row. The natural width is the floor —
        // no extra "separator" padding (issue #42). Flush spacing on
        // EVERY row (`⇄18`, not `⇄  18`) is then a property of the
        // column being LEFT-aligned: the deficit pads on the right,
        // after the number, so the glyph never gets a leading gap
        // regardless of digit count. A right-aligned column padded the
        // short rows on the left and reopened that gap — issue #65. The
        // role cell that follows brings its own leading space.
        let max_pr_num_width = self
            .visible
            .iter()
            .filter_map(|row| match row {
                VisibleRow::Workspace(k) => self
                    .workspaces
                    .get(k)
                    .and_then(|w| w.primary_task())
                    .and_then(crate::components::task_label::pr_number)
                    .map(|n| 1 + n.checked_ilog10().unwrap_or(0) as usize),
                _ => None,
            })
            .max()
            .unwrap_or(1);
        // Column spec for workspace rows — built once per render
        // (max_pr_num_width is fixed across rows in this pass).
        let workspace_columns = crate::components::workspace_row::build_columns(max_pr_num_width);

        // Workspace rows go through ONE `render_table` call so
        // `Column::max(0)` sees every row's natural cell width and
        // picks a single column width for all of them. When each
        // row was rendered solo, an empty status / badge cell
        // collapsed THAT row's column to 0 while a sibling row kept
        // its full width — the `C` badge visibly drifted between
        // lines and the title flex absorbed different amounts per
        // row.
        let mut rendered_workspace_lines = self.prebuild_workspace_lines(
            &workspace_columns,
            max_pr_num_width,
            row_budget,
            theme,
            now,
            focused,
        );

        let lines: Vec<Line> = self
            .visible
            .iter()
            .enumerate()
            .map(|(i, row)| match row {
                VisibleRow::RepoHeader(name) => {
                    use crate::components::icons;
                    let collapsed = self.collapsed_repos.contains(name);
                    let glyph = if collapsed { "▸" } else { "▾" };
                    let is_cursor = i == self.cursor;
                    let row_bg = if is_cursor && focused {
                        Some(theme.row_focused())
                    } else if is_cursor {
                        Some(theme.row_unfocused())
                    } else {
                        None
                    };
                    // Root of the tree, so it carries no selection
                    // caret: the disclosure glyph sits in the shared
                    // left gutter and the cursor is shown by the
                    // row-background fill below. That keeps top-level
                    // rows nearly flush with the pane edge (issue #231).
                    let glyph_style = match row_bg {
                        Some(bg) => bg,
                        None => Style::default().fg(theme.text_dim),
                    };
                    let mut spans: Vec<Span> = vec![
                        Span::styled(format!("{glyph} "), glyph_style),
                        Span::styled(
                            format!("{} {}", icons::REPO, name),
                            row_bg
                                .unwrap_or_default()
                                .fg(theme.warn)
                                .add_modifier(Modifier::BOLD),
                        ),
                    ];
                    if let Some(s) = self.repo_summaries.get(name) {
                        // Active count is redundant — the workspace
                        // rows are visible directly under the header,
                        // so the user can count them. The attention
                        // pill is the only summary that adds info
                        // (and only when non-zero). Two raw numbers
                        // side-by-side looked like a broken counter.
                        if s.attention > 0 {
                            spans.push(Span::styled(
                                format!("  ● {}", s.attention),
                                row_bg
                                    .unwrap_or_default()
                                    .fg(theme.hover)
                                    .add_modifier(Modifier::BOLD),
                            ));
                        }
                        // While a search filters THIS project, surface
                        // the match count so the header reads as a
                        // result tally rather than a vanished tree.
                        if self
                            .search
                            .as_ref()
                            .is_some_and(|q| !q.query.is_empty() && q.scope == *name)
                        {
                            spans.push(Span::styled(
                                format!("  {} match", s.active),
                                row_bg
                                    .unwrap_or_default()
                                    .fg(theme.accent)
                                    .add_modifier(Modifier::BOLD),
                            ));
                        }
                    }
                    // Without a caret, the cursor reads purely from the
                    // row-background fill, so extend it across the whole
                    // row rather than leaving it hugging the text.
                    if let Some(bg) = row_bg {
                        extend_cursor_fill(&mut spans, row_budget, bg);
                    }
                    Line::from(spans)
                }
                VisibleRow::KindHeader(kind) => {
                    // PR/Issue section header, sitting between the repo
                    // header and the workspace rows of that kind. The
                    // chip-coloured marker mirrors the per-row PR/issue
                    // pills and sits at the same inset as the workspace
                    // type glyph so the eye lines them up.
                    let is_cursor = i == self.cursor;
                    let row_bg = if is_cursor && focused {
                        Some(theme.row_focused())
                    } else if is_cursor {
                        Some(theme.row_unfocused())
                    } else {
                        None
                    };
                    let caret = if is_cursor { "▸" } else { " " };
                    let color = match kind {
                        WorkspaceKind::Pr => theme.success,
                        WorkspaceKind::Issue => theme.hover,
                        WorkspaceKind::Other => theme.text_dim,
                    };
                    let marker = kind.header_marker();
                    let label = kind.header_label();
                    let mut spans: Vec<Span> = vec![
                        Span::styled(
                            caret.to_string(),
                            row_bg.unwrap_or_default().fg(theme.text_dim),
                        ),
                        Span::styled(format!("{marker} "), row_bg.unwrap_or_default().fg(color)),
                        Span::styled(
                            label,
                            row_bg
                                .unwrap_or_default()
                                .fg(color)
                                .add_modifier(Modifier::BOLD),
                        ),
                    ];
                    if let Some(bg) = row_bg {
                        extend_cursor_fill(&mut spans, row_budget, bg);
                    }
                    Line::from(spans)
                }
                VisibleRow::Workspace(_) => rendered_workspace_lines
                    .get_mut(i)
                    .and_then(|slot| slot.take())
                    .unwrap_or_default(),
                VisibleRow::Session {
                    workspace,
                    session_id,
                } => {
                    // Per-session sub-row, only emitted when the
                    // workspace has 2+ sessions. One indent step deeper
                    // than the workspace row (shared caret gutter + a
                    // 2-col nesting step) before the session name.
                    let name = self
                        .workspaces
                        .get(workspace)
                        .and_then(|w| w.find_session(*session_id))
                        .map(|s| s.name.as_str())
                        .unwrap_or("?");
                    let is_cursor = i == self.cursor;
                    let style = if is_cursor && focused {
                        theme.row_focused()
                    } else if is_cursor {
                        theme.row_unfocused()
                    } else {
                        Style::default().fg(theme.text_dim)
                    };
                    let prefix = if is_cursor { "▸  " } else { "   " };
                    let name_budget = row_budget.saturating_sub(visual_width(prefix));
                    let name_text = truncate_ellipsis(name, name_budget);
                    let mut spans =
                        vec![Span::styled(prefix, style), Span::styled(name_text, style)];
                    if is_cursor {
                        extend_cursor_fill(&mut spans, row_budget, style);
                    }
                    Line::from(spans)
                }
            })
            .collect();

        // Row-window the list. Each `VisibleRow` is exactly one line,
        // so the scroll offset is a plain row count — clamp it to keep
        // `cursor` in view, then bound it to the tail so the last rows
        // can't scroll past the bottom edge. A wheel-detached viewport
        // skips the cursor clamp: the wheel only moves the display,
        // and snapping back here would undo it on the next frame. The
        // detach flag is cleared by explicit cursor moves, so j/k et
        // al. still re-anchor.
        let total_rows = lines.len();
        let viewport = inner.height as usize;
        if !self.scroll_detached {
            if self.cursor < self.scroll {
                self.scroll = self.cursor;
            } else if viewport > 0 && self.cursor >= self.scroll + viewport {
                self.scroll = self.cursor + 1 - viewport;
            }
        }
        let max_scroll = total_rows.saturating_sub(viewport);
        if self.scroll > max_scroll {
            self.scroll = max_scroll;
        }
        self.last_viewport = viewport;
        self.rendered_scroll = self.scroll;

        let para = Paragraph::new(lines).scroll((self.scroll as u16, 0));
        frame.render_widget(para, inner);

        // First-run / empty-inbox guidance. When the list is genuinely
        // empty (default Inbox, no filter, no search) the blank pane is
        // a dead end — replace it with a panel that names the next
        // actions, foregrounding the worktree-session flow that works
        // with zero GitHub data (issue #100).
        if self.is_getting_started() {
            self.render_getting_started(inner, frame, theme);
        }

        // Scroll-position indicator in the right padding strip —
        // auto-hides when the whole list fits.
        crate::components::scrollbar::render_vertical(
            frame,
            Rect::new(
                area.x + area.width.saturating_sub(1),
                inner.y,
                1,
                inner.height,
            ),
            total_rows,
            viewport,
            self.scroll,
        );

        // Bottom search bar (fzf-style). `/query` while editing, with a
        // caret; once `Enter` keeps the filter applied the caret drops
        // and a hint reminds the user `/` re-edits and `esc` clears.
        if search_bar && let Some(s) = self.search.as_ref() {
            let bar = Rect::new(area.x + l_pad, area.y + area.height - 1, inner_width, 1);
            let mut spans = vec![
                Span::styled("/", Style::default().fg(theme.accent)),
                Span::styled(s.query.clone(), Style::default().fg(theme.text_strong)),
            ];
            if s.editing {
                spans.push(Span::styled("▏", Style::default().fg(theme.accent)));
                spans.push(Span::styled(
                    "  esc clear",
                    Style::default().fg(theme.text_dim),
                ));
            } else {
                spans.push(Span::styled(
                    "  / edit · esc clear",
                    Style::default().fg(theme.text_dim),
                ));
            }
            frame.render_widget(Paragraph::new(Line::from(spans)), bar);
        }
    }

    /// Paint the empty-inbox getting-started panel into the content
    /// area. Leads with the worktree-session flow (which needs no
    /// GitHub data) and closes with the orientation shortcuts, so a
    /// new user with an empty inbox has somewhere to go (issue #100).
    fn render_getting_started(&self, inner: Rect, frame: &mut Frame, theme: &crate::theme::Theme) {
        if inner.height < 4 {
            return;
        }
        let heading = Style::default()
            .fg(theme.text_strong)
            .add_modifier(Modifier::BOLD);
        let prose = Style::default().fg(theme.text_dim);
        let key = Style::default()
            .fg(theme.accent)
            .add_modifier(Modifier::BOLD);
        let label = Style::default().fg(theme.text_dim);
        // `key` (left-padded to a column) + label, one shortcut per row.
        let hint = |k: &str, text: &str| {
            Line::from(vec![
                Span::styled(format!("  {k:<5}"), key),
                Span::styled(text.to_string(), label),
            ])
        };

        let lines: Vec<Line<'static>> = vec![
            Line::raw(""),
            Line::from(Span::styled(" No PRs or issues yet", heading)),
            Line::raw(""),
            Line::from(Span::styled(" lazybox also manages your", prose)),
            Line::from(Span::styled(" git worktrees — spin up an", prose)),
            Line::from(Span::styled(" agent session per task:", prose)),
            Line::raw(""),
            hint("⇧W", "start work"),
            hint("x p", "new project"),
            hint("x n", "new workspace"),
            Line::raw(""),
            Line::from(Span::styled(" or open a tool yourself:", prose)),
            hint("a c", "claude"),
            hint("s", "shell"),
            hint("e", "editor"),
            Line::raw(""),
            Line::from(Span::styled(" new here?", prose)),
            hint("?", "Ask Lazybox"),
            hint("⇧T", "tour"),
            hint("⇧R", "refresh inbox"),
        ];
        frame.render_widget(Paragraph::new(lines), inner);
    }

    /// Build & lay out every visible workspace row in one
    /// `render_table` pass, then scatter the resulting Lines back to
    /// the visible-list indices they belong to.
    ///
    /// The returned `Vec<Option<Line>>` has `self.visible.len()`
    /// slots, with `Some(line)` at every `VisibleRow::Workspace`
    /// position and `None` everywhere else. The caller `.take()`s
    /// each Line as it walks `self.visible`, so every Line is moved
    /// exactly once.
    ///
    /// This is what fixes issue #22's column-drift: each `Max`
    /// column in `workspace_columns` picks one width across all
    /// rows in this call, instead of collapsing per-row whenever a
    /// row happened to have an empty cell there.
    fn prebuild_workspace_lines(
        &self,
        workspace_columns: &[crate::components::table::Column],
        max_pr_num_width: usize,
        row_budget: usize,
        theme: &crate::theme::Theme,
        now: chrono::DateTime<chrono::Utc>,
        focused: bool,
    ) -> Vec<Option<Line<'static>>> {
        let workspace_count = self
            .visible
            .iter()
            .filter(|r| matches!(r, VisibleRow::Workspace(_)))
            .count();
        // 1-based jump numbers for the first nine agent workspaces, in
        // sidebar order — the badge that pairs with the `]]<digit>`
        // jump. Past the ninth there's no single-digit key, so it gets
        // no badge.
        let agent_numbers: std::collections::HashMap<SessionKey, usize> = self
            .agent_workspace_keys()
            .into_iter()
            .take(9)
            .enumerate()
            .map(|(i, k)| (k, i + 1))
            .collect();
        let mut positions: Vec<usize> = Vec::with_capacity(workspace_count);
        let mut rows: Vec<TableRow> = Vec::with_capacity(workspace_count);
        for (i, row) in self.visible.iter().enumerate() {
            let VisibleRow::Workspace(key) = row else {
                continue;
            };
            let workspace = self.workspaces.get(key);
            let ctx = WorkspaceRowCtx {
                workspace,
                task: workspace.and_then(|w| w.primary_task()),
                theme,
                now,
                focused,
                is_cursor: i == self.cursor,
                is_selected: self.broadcast_selected.contains(key),
                max_pr_num_width,
                asking: workspace
                    .is_some_and(|w| crate::agent_attention::workspace_is_asking(w, &self.agents)),
                working: workspace
                    .is_some_and(|w| crate::agent_attention::workspace_is_working(w, &self.agents)),
                done: workspace
                    .is_some_and(|w| crate::agent_attention::workspace_is_done(w, &self.agents)),
                exited: workspace
                    .is_some_and(|w| crate::agent_attention::workspace_is_exited(w, &self.agents)),
                working_glyph: crate::components::workspace_row::working_glyph(
                    self.working_spinner_frame,
                ),
                badges: self.runner_badges(key),
                agent_number: agent_numbers.get(key).copied(),
                ascii_glyphs: self.ascii_glyphs,
                auto_merge_armed: workspace.is_some_and(|w| w.auto_merge_on_green),
                auto_fix_armed: workspace.is_some_and(|w| w.policies.any_auto_fix_armed()),
                has_notes: workspace.is_some_and(|w| w.has_notes()),
            };
            positions.push(i);
            rows.push(build_workspace_row(&ctx));
        }
        let lines = crate::components::table::render_table(&rows, workspace_columns, row_budget);
        let mut out: Vec<Option<Line<'static>>> = vec![None; self.visible.len()];
        for (i, line) in positions.into_iter().zip(lines) {
            out[i] = Some(line);
        }
        out
    }
}
