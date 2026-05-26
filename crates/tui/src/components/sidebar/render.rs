//! The sidebar's `render` method. Pulled into its own file because
//! it's 280 lines on its own — the V1-style header strip + the
//! visible-rows render loop + the click-hit-test population each
//! have non-trivial layout logic, and inlining them next to the
//! key-handler made the parent `impl` block hard to navigate.

use super::*;

impl Sidebar {
    pub fn render(&mut self, area: Rect, frame: &mut Frame, focused: bool) {
        // V1-style header strip:
        //   row 0: PILOT  N  ● N new  ? N input  [7d]
        //   row 1: s  filter (needs:reply ci:failed ...)
        //   row 2: N CI  N review               (omitted when both 0)
        //   row 3: ── divider ────────────────
        //   row 4: blank
        //   row 5+: content
        let theme = crate::theme::current();
        let now = chrono::Utc::now();
        let mailbox_label = match self.mailbox {
            Mailbox::Inbox => "PILOT",
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

        // Row 1 — filter hint.
        if area.height >= 2 {
            let row1 = Rect::new(area.x + l_pad, area.y + 1, inner_width, 1);
            frame.render_widget(
                Paragraph::new(Line::from(vec![
                    Span::styled(
                        "/ ",
                        Style::default()
                            .fg(theme.accent)
                            .add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(
                        "filter (needs:reply ci:failed …)",
                        Style::default().fg(theme.text_dim),
                    ),
                ])),
                row1,
            );
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
        // room above the first item).
        const HEADER_HEIGHT: u16 = 5;
        let inner = Rect {
            x: area.x + l_pad,
            y: area.y + HEADER_HEIGHT,
            width: inner_width,
            height: area.height.saturating_sub(HEADER_HEIGHT),
        };

        let row_budget = inner_width as usize;
        // Pre-pass: compute the widest `#NNN` across visible workspace
        // rows so every row pads to the same column. Without this,
        // `#7204 R` and `#31 R` had different role-letter positions
        // and the whole column visibly jittered. Minimum 3 ("#NN")
        // so very-short numbers still leave space for a separator.
        let max_pr_num_width = self
            .visible
            .iter()
            .filter_map(|row| match row {
                VisibleRow::Workspace(k) => self
                    .workspaces
                    .get(k)
                    .and_then(|w| w.primary_task())
                    .and_then(crate::components::task_label::pr_number)
                    .map(|n| format!("#{n}").chars().count()),
                _ => None,
            })
            .max()
            .unwrap_or(3)
            .max(3);
        // Column spec for workspace rows — built once per render
        // (max_pr_num_width is fixed across rows in this pass).
        // Each row's `render_table` call reuses this slice; the
        // table primitive owns padding + cursor fill geometry.
        let workspace_columns = crate::components::workspace_row::build_columns(max_pr_num_width);
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
                    }
                    let _ = row_budget;
                    Line::from(spans)
                }
                VisibleRow::Workspace(key) => {
                    use crate::components::workspace_row::{WorkspaceRowCtx, build_row};
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
                        badges: self.runner_badges(key),
                    };
                    let row = build_row(&ctx);
                    crate::components::table::render_table(&[row], &workspace_columns, row_budget)
                        .into_iter()
                        .next()
                        .unwrap_or_default()
                }
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

        let para = Paragraph::new(lines);
        frame.render_widget(para, inner);
    }
}
