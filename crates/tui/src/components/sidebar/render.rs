//! The sidebar's `render` method. Pulled into its own file because
//! it's 280 lines on its own — the V1-style header strip + the
//! visible-rows render loop + the click-hit-test population each
//! have non-trivial layout logic, and inlining them next to the
//! key-handler made the parent `impl` block hard to navigate.

use super::*;
use crate::components::table::Row as TableRow;
use crate::components::workspace_row::{WorkspaceRowCtx, build_row as build_workspace_row};

impl Sidebar {
    pub fn render(&mut self, area: Rect, frame: &mut Frame, focused: bool) {
        // V1-style header strip:
        //   row 0: LAZYBOX  N  ● N new  ? N input  [7d]
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

        let l_pad: u16 = 1;
        let r_pad: u16 = 3;
        let inner_width = area.width.saturating_sub(l_pad + r_pad);

        // Row 0 — app title + counts.
        let mut header_spans: Vec<Span> = Vec::with_capacity(12);
        header_spans.push(Span::styled(mailbox_label, theme.title(focused)));
        header_spans.push(Span::raw("  "));
        header_spans.push(Span::styled(
            count.to_string(),
            Style::default().fg(theme.warn).add_modifier(Modifier::BOLD),
        ));
        if unread > 0 {
            header_spans.push(Span::raw("  "));
            header_spans.push(Span::styled(
                "● ",
                Style::default()
                    .fg(theme.hover)
                    .add_modifier(Modifier::BOLD),
            ));
            header_spans.push(Span::styled(
                format!("{unread} new"),
                Style::default()
                    .fg(theme.hover)
                    .add_modifier(Modifier::BOLD),
            ));
        }
        if input_pending > 0 {
            header_spans.push(Span::raw("  "));
            header_spans.push(Span::styled(
                "? ",
                Style::default().fg(theme.warn).add_modifier(Modifier::BOLD),
            ));
            header_spans.push(Span::styled(
                format!("{input_pending} input"),
                Style::default().fg(theme.warn).add_modifier(Modifier::BOLD),
            ));
        }
        header_spans.push(Span::raw("  "));
        header_spans.push(Span::styled("[7d]", Style::default().fg(theme.text_dim)));

        let row0 = Rect::new(area.x + l_pad, area.y, inner_width, 1.min(area.height));
        frame.render_widget(Paragraph::new(Line::from(header_spans)), row0);

        // Row 1 — role filter + sort chips, both clickable. Each
        // chip is dim while at its default, accent-bold when the
        // user has selected a non-default value, so the row stays
        // quiet by default but visually shouts when a filter is on.
        if area.height >= 2 {
            let row1 = Rect::new(area.x + l_pad, area.y + 1, inner_width, 1);

            let filter_active = self.role_filter != RoleFilter::All;
            let sort_active = self.sort_mode != SortMode::Recent;
            let active_style = Style::default()
                .fg(theme.accent)
                .add_modifier(Modifier::BOLD);
            let dim_style = Style::default().fg(theme.text_dim);
            let key_style = active_style;

            let filter_prefix = "f ";
            let filter_chip = format!("[{}]", self.role_filter.chip_label());
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

        // Row 3 — thin grey divider.
        if area.height >= 4 {
            let div_area = Rect::new(area.x + l_pad, area.y + 3, inner_width, 1);
            let divider = "─".repeat(div_area.width as usize);
            frame.render_widget(
                Paragraph::new(Line::from(Span::styled(divider, theme.divider()))),
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
                    // Cursor caret on the left mirrors workspace rows so
                    // the user can see the cursor parked on a header
                    // (otherwise navigating onto a header looks like a
                    // dropped key — Space-to-toggle wouldn't be
                    // discoverable).
                    let caret = if is_cursor { "▸ " } else { "  " };
                    let glyph_style = match row_bg {
                        Some(bg) => bg,
                        None => Style::default().fg(theme.text_dim),
                    };
                    let mut spans: Vec<Span> = vec![
                        Span::styled(caret.to_string(), glyph_style),
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
                    let _ = row_budget;
                    Line::from(spans)
                }
                VisibleRow::KindHeader(kind) => {
                    // Indented PR/Issue section header, sitting
                    // between the repo header and the workspace rows
                    // of that kind. Distinct from `RepoHeader` by
                    // indent + leading marker; the chip-coloured
                    // marker mirrors the per-row PR/issue pills so
                    // the eye lines them up.
                    let is_cursor = i == self.cursor;
                    let row_bg = if is_cursor && focused {
                        Some(theme.row_focused())
                    } else if is_cursor {
                        Some(theme.row_unfocused())
                    } else {
                        None
                    };
                    let caret = if is_cursor { "▸ " } else { "  " };
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
                        // Two-space indent so kind headers tuck under
                        // their parent repo header visually.
                        Span::raw("  "),
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
                        // caret + 2-space indent + "X " marker + label.
                        let used = caret.chars().count() + 2 + 2 + label.chars().count();
                        if used < row_budget {
                            spans.push(Span::styled(" ".repeat(row_budget - used), bg));
                        }
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
                    // workspace has 2+ sessions. Indent further under
                    // the workspace row and show the session name.
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
                    let prefix = if is_cursor { "      ▸ " } else { "        " };
                    let name_budget = row_budget.saturating_sub(visual_width(prefix));
                    let name_text = truncate_ellipsis(name, name_budget);
                    let used = visual_width(prefix) + visual_width(&name_text);
                    let mut spans =
                        vec![Span::styled(prefix, style), Span::styled(name_text, style)];
                    if is_cursor && used < row_budget {
                        spans.push(Span::styled(" ".repeat(row_budget - used), style));
                    }
                    Line::from(spans)
                }
            })
            .collect();

        // Row-window the list so the cursor stays on screen. Each
        // `VisibleRow` is exactly one line, so the scroll offset is a
        // plain row count — clamp it to keep `cursor` in view, then
        // bound it to the tail so the last rows can't scroll past the
        // bottom edge.
        let total_rows = lines.len();
        let viewport = inner.height as usize;
        if self.cursor < self.scroll {
            self.scroll = self.cursor;
        } else if viewport > 0 && self.cursor >= self.scroll + viewport {
            self.scroll = self.cursor + 1 - viewport;
        }
        let max_scroll = total_rows.saturating_sub(viewport);
        if self.scroll > max_scroll {
            self.scroll = max_scroll;
        }

        let para = Paragraph::new(lines).scroll((self.scroll as u16, 0));
        frame.render_widget(para, inner);

        // Scroll-position indicator in the right padding strip —
        // auto-hides when the whole list fits.
        crate::components::scrollbar::render_vertical(
            frame,
            Rect::new(
                area.x + area.width.saturating_sub(2),
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
                max_pr_num_width,
                long_snooze_armed: self.latches.armed(TRIGGER_LONG_SNOOZE) == Some(key),
                asking: workspace.is_some_and(|w| {
                    crate::agent_attention::workspace_is_asking(w, &self.agents_asking)
                }),
                working: workspace.is_some_and(|w| {
                    crate::agent_attention::workspace_is_working(w, &self.agents_working)
                }),
                working_glyph: crate::components::workspace_row::working_glyph(
                    self.working_spinner_frame,
                ),
                badges: self.runner_badges(key),
                ascii_glyphs: self.ascii_glyphs,
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
