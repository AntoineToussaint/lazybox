//! The sidebar's `render` method. Pulled into its own file because
//! it's 280 lines on its own — the V1-style header strip + the
//! visible-rows render loop + the click-hit-test population each
//! have non-trivial layout logic, and inlining them next to the
//! key-handler made the parent `impl` block hard to navigate.

use super::*;
use crate::components::table::Row as TableRow;
use crate::components::workspace_row::{WorkspaceRowCtx, build_row as build_workspace_row};
use lazybox_tui_core::usage::{UsageSpanKind, UsageSummary};

/// Style-and-join the per-provider usage summaries into one header line,
/// width-gated: a provider whose whole widget won't fit `inner_width` is
/// dropped rather than clipped mid-glyph (like the row-2 stats groups).
/// Empty when there is nothing to show, which the caller reads as "no
/// usage row".
fn usage_line_spans(
    summaries: &[UsageSummary],
    inner_width: usize,
    theme: &crate::theme::Theme,
) -> Vec<Span<'static>> {
    let mut out: Vec<Span<'static>> = Vec::new();
    let mut used = 0usize;
    for summary in summaries {
        let group: Vec<Span<'static>> = summary
            .spans()
            .into_iter()
            .map(|(text, kind)| {
                let style = match kind {
                    UsageSpanKind::Label => Style::default()
                        .fg(theme.accent)
                        .add_modifier(Modifier::BOLD),
                    UsageSpanKind::BarFilled => Style::default().fg(theme.accent),
                    UsageSpanKind::BarEmpty => Style::default().fg(theme.text_dim),
                    UsageSpanKind::Figure => Style::default().add_modifier(Modifier::BOLD),
                    UsageSpanKind::Meta => Style::default().fg(theme.text_dim),
                    UsageSpanKind::Reset => {
                        Style::default().fg(theme.warn).add_modifier(Modifier::BOLD)
                    }
                };
                Span::styled(text, style)
            })
            .collect();
        let group_width = spans_visual_width(&group);
        let sep = if out.is_empty() { 0 } else { 2 };
        if used + sep + group_width > inner_width {
            continue;
        }
        if sep > 0 {
            out.push(Span::raw("  "));
        }
        out.extend(group);
        used += sep + group_width;
    }
    out
}

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
    /// Height the always-visible usage row adds to the header for a pane
    /// of this width — `1` when it renders, `0` otherwise. The click
    /// hit-tests fold this into their header offset so a click still lands
    /// on the row actually drawn once the usage row shifts content down.
    pub(super) fn usage_row_height(&self, area: Rect) -> u16 {
        let inner_width = area.width.saturating_sub(2) as usize;
        let theme = crate::theme::current();
        if usage_line_spans(&self.usage_summaries(), inner_width, theme).is_empty() {
            0
        } else {
            1
        }
    }

    pub fn render(&mut self, area: Rect, frame: &mut Frame, focused: bool) {
        // V1-style header strip:
        //   row 0: LAZYBOX vX.Y.Z  ● N new  ? N input        N items  7d
        //   row 1: s  filter (needs:reply ci:failed ...)
        //   row 2: <focused merge/auto-fix automation>  N CI  N review
        //          (each group width-gated; omitted when it can't fit)
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
        // Broadcast multi-select count — only the *visible* marks, so the
        // header agrees with the on-screen `✓` gutter and with what a
        // broadcast will target (issue #786). Placed ahead of the passive
        // badges below and fitted on its own (see the header assembly), so
        // the live mode indicator survives a narrow sidebar.
        let selected = self.visible_broadcast_selected_count();
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
        // Usage-limit indicator (#1012): the count of workspaces whose
        // agent is blocked on its provider usage limit, so the proactive
        // "act before more hit the wall" signal reads at a glance next to
        // the `?` input count. Same `⏳` glyph as the per-row pill.
        let limited = self.limit_reached_workspace_count();
        if limited > 0 {
            if !signal_spans.is_empty() {
                signal_spans.push(Span::raw("  "));
            }
            signal_spans.push(Span::styled(
                "⏳ ",
                Style::default().fg(theme.warn).add_modifier(Modifier::BOLD),
            ));
            signal_spans.push(Span::styled(
                format!("{limited} limited"),
                Style::default().fg(theme.warn).add_modifier(Modifier::BOLD),
            ));
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

        // The live multi-select count is the highest-priority signal —
        // it's the mode you're actively in — so it's placed first and
        // fitted on its own budget. That way it survives even when the
        // passive badges below don't, instead of the whole signal strip
        // dropping as an all-or-nothing block (issue #786).
        let mut chip_placed = false;
        if selected > 0 {
            let chip = Span::styled(
                format!("✓ {selected} selected"),
                Style::default()
                    .fg(theme.accent)
                    .add_modifier(Modifier::BOLD),
            );
            let chip_width = spans_visual_width(std::slice::from_ref(&chip));
            let left_width = spans_visual_width(&header_spans);
            if inner_width as usize >= left_width + 2 + chip_width + 2 + summary_width {
                header_spans.push(Span::raw("  "));
                header_spans.push(chip);
                chip_placed = true;
            }
        }

        // Passive badges (`● new`, `? input`, `☼ awake`) are lower
        // priority than the selection count. When a selection is active
        // but its count didn't fit, suppress them too: a shorter badge
        // must never take the count's place, which would read as "nothing
        // selected" while marks are live (issue #786).
        let show_passive = selected == 0 || chip_placed;
        let signal_width = spans_visual_width(&signal_spans);
        let current_width = spans_visual_width(&header_spans);
        if show_passive
            && !signal_spans.is_empty()
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

            let mut spans = vec![
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
            ];

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

            // Always-visible global search box, to the right of the
            // sort chip: `# [find]` when idle, `# [⌕ query]` (accent,
            // truncated) while a scope-less query is applied. Clicking
            // it opens the global search (#755). Dropped only when the
            // pane is too narrow to fit even the placeholder.
            let global_query = self
                .search
                .as_ref()
                .filter(|s| s.scope.is_none() && !s.query.is_empty())
                .map(|s| s.query.as_str());
            let search_prefix = "# ";
            let search_prefix_cells = visual_width(search_prefix) as u16;
            let sort_end = sort_x + sort_w;
            let search_x = sort_end + sep_cells;
            let box_start = search_x + search_prefix_cells;
            // Cells left for the chip body after the leading `# `.
            let chip_room = (row1.x + row1.width).saturating_sub(box_start);
            // The `[⌕ …]` frame around the query; its width is measured
            // rather than assumed so a wider search glyph can't push the
            // chip past the room the guard below reserved for it.
            const SEARCH_FRAME: &str = "[⌕ ]";
            let search_chip = match global_query {
                Some(q) => {
                    let q_room =
                        chip_room.saturating_sub(visual_width(SEARCH_FRAME) as u16) as usize;
                    format!("[⌕ {}]", truncate_ellipsis(q, q_room.max(1)))
                }
                None => "[find]".to_string(),
            };
            let search_chip_cells = visual_width(&search_chip) as u16;
            if search_chip_cells <= chip_room {
                spans.push(Span::raw(sep));
                spans.push(Span::styled(search_prefix, key_style));
                spans.push(Span::styled(
                    search_chip,
                    if global_query.is_some() {
                        active_style
                    } else {
                        dim_style
                    },
                ));
                self.search_chip_rect = Some(Rect {
                    x: search_x,
                    y: row1.y,
                    width: search_prefix_cells + search_chip_cells,
                    height: 1,
                });
            } else {
                self.search_chip_rect = None;
            }

            frame.render_widget(Paragraph::new(Line::from(spans)), row1);
        } else {
            self.filter_chip_rect = None;
            self.sort_chip_rect = None;
            self.search_chip_rect = None;
        }

        // Row 2 — focused automation plus stats summary. Assembled
        // width-aware (like the row-0 summary): each group is appended only
        // if it fits whole in the header, so a lower-priority group drops
        // cleanly rather than a hard clip slicing a label mid-word. That
        // matters most for the merge-automation phrase (#794) — a truncated
        // " AUTO-MERGE · GitHub, works offli…" would drop exactly the
        // durability word that is the point of the label. Priority, highest
        // first: focused merge automation, then armed auto-fix, then the
        // global CI / review tallies. The focused row's own automation
        // outranks the global tallies deliberately — it is the context for
        // the row under the cursor, not an inbox-wide count.
        let focused_workspace = self.visible.get(self.cursor).and_then(|row| match row {
            VisibleRow::Workspace(key) => self.workspaces.get(key),
            _ => None,
        });
        // Spell out the focused row's merge automation in words — the
        // compact ` ARM ` / ` AUTO ` pills look alike but guarantee
        // different things (#794). GitHub-native auto-merge wins when both
        // are set (it merges server-side, even with lazybox closed), so it
        // takes the label; otherwise the lazybox arm names its while-running
        // limit. Colored to match each pill: accent for GitHub, green for
        // the lazybox arm.
        let focused_merge = focused_workspace.and_then(|workspace| {
            if workspace
                .pr
                .as_ref()
                .is_some_and(|pr| pr.auto_merge_enabled)
            {
                Some((" AUTO-MERGE · GitHub, works offline ", theme.accent))
            } else if workspace.auto_merge_on_green {
                Some((" MERGE ON GREEN · lazybox only ", theme.success))
            } else {
                None
            }
        });
        let focused_auto_fix = focused_workspace.and_then(|workspace| {
            let ci = workspace.policies.arm(lazybox_core::AutoFixKind::CiFailure)
                == lazybox_core::PolicyArm::Arm;
            let conflict = workspace
                .policies
                .arm(lazybox_core::AutoFixKind::MergeConflict)
                == lazybox_core::PolicyArm::Arm;
            match (ci, conflict) {
                (true, true) => Some(" AUTO-FIX ON · CI+CONFLICT "),
                (true, false) => Some(" AUTO-FIX ON · CI FAIL "),
                (false, true) => Some(" AUTO-FIX ON · CONFLICT "),
                (false, false) => None,
            }
        });

        // Append `group` (with a 2-cell separator once the line is
        // non-empty) only when the whole group still fits `budget`, so a
        // group never renders as a mid-content fragment.
        fn try_append<'a>(
            dst: &mut Vec<Span<'a>>,
            used: &mut usize,
            budget: usize,
            group: Vec<Span<'a>>,
        ) {
            let sep = if dst.is_empty() { 0 } else { 2 };
            let width: usize = group
                .iter()
                .map(|span| visual_width(span.content.as_ref()))
                .sum();
            if *used + sep + width > budget {
                return;
            }
            if sep > 0 {
                dst.push(Span::raw("  "));
            }
            dst.extend(group);
            *used += sep + width;
        }

        let mut stats_spans: Vec<Span> = Vec::new();
        let mut used = 0usize;
        let budget = inner_width as usize;
        if let Some((label, bg)) = focused_merge {
            try_append(
                &mut stats_spans,
                &mut used,
                budget,
                vec![Span::styled(
                    label,
                    Style::default()
                        .bg(bg)
                        .fg(ratatui::style::Color::Black)
                        .add_modifier(Modifier::BOLD),
                )],
            );
        }
        if let Some(label) = focused_auto_fix {
            try_append(
                &mut stats_spans,
                &mut used,
                budget,
                vec![Span::styled(
                    label,
                    Style::default()
                        .bg(theme.warn)
                        .fg(ratatui::style::Color::Black)
                        .add_modifier(Modifier::BOLD),
                )],
            );
        }
        if ci_failing > 0 {
            try_append(
                &mut stats_spans,
                &mut used,
                budget,
                vec![
                    Span::styled(
                        ci_failing.to_string(),
                        Style::default()
                            .fg(theme.error)
                            .add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(" CI", Style::default().fg(theme.text_dim)),
                ],
            );
        }
        if review_pending > 0 {
            try_append(
                &mut stats_spans,
                &mut used,
                budget,
                vec![
                    Span::styled(
                        review_pending.to_string(),
                        Style::default()
                            .fg(theme.accent)
                            .add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(" review", Style::default().fg(theme.text_dim)),
                ],
            );
        }
        if !stats_spans.is_empty() && area.height >= 3 {
            let row2 = Rect::new(area.x + l_pad, area.y + 2, inner_width, 1);
            frame.render_widget(Paragraph::new(Line::from(stats_spans)), row2);
        }

        // Row 3 (when present) — the always-visible per-provider usage
        // summary (#1059). One compact `Claude ▓▓▓░░ 62% · 76k left`
        // widget per agent with a live terminal, width-gated the same way
        // as the stats row: a provider that can't fit whole is dropped
        // rather than sliced. Sits above the divider so it reads as header
        // chrome, not a list row; absent (no agents, or `ui.usage_summary`
        // off) it takes no space and the layout is unchanged.
        let usage_spans = usage_line_spans(&self.usage_summaries(), inner_width as usize, theme);
        let usage_h: u16 = if usage_spans.is_empty() { 0 } else { 1 };
        if usage_h == 1 && area.height >= 4 {
            let usage_area = Rect::new(area.x + l_pad, area.y + 3, inner_width, 1);
            frame.render_widget(Paragraph::new(Line::from(usage_spans)), usage_area);
        }

        // Divider — thin, accent-tinted while this pane has focus so the
        // active pane reads at a glance (#286). Sits just under the usage
        // row (or row 2 when there is none).
        let divider_y = 3 + usage_h;
        if area.height >= 4 + usage_h {
            let div_area = Rect::new(area.x + l_pad, area.y + divider_y, inner_width, 1);
            let divider = "─".repeat(div_area.width as usize);
            frame.render_widget(
                Paragraph::new(Line::from(Span::styled(
                    divider,
                    theme.pane_divider(focused),
                ))),
                div_area,
            );
        }

        // Content starts one blank row below the divider (breathing room
        // above the first item). When the `/` search bar is open it claims
        // the bottom row, so the list loses one line — the bar is pinned to
        // the bottom (fzf-style) so the repo tree doesn't shift as the user
        // types.
        let header_height: u16 = 5 + usage_h;
        let search_bar = self.search.is_some() && area.height > header_height;
        let inner = Rect {
            x: area.x + l_pad,
            y: area.y + header_height,
            width: inner_width,
            height: area
                .height
                .saturating_sub(header_height + u16::from(search_bar)),
        };

        let row_budget = inner_width as usize;
        // Workspace rows are laid out per group, not globally: each repo /
        // provider group runs its own column pre-pass over just its own
        // rows. GitHub's `#NNN` and Linear's `TEAM-NNNN` are different
        // shapes, and their status columns differ (GitHub has CI / review,
        // Linear doesn't) — a single global layout let one provider's
        // widest reference bloat the other's column and reserved
        // provider-specific status columns for groups that never fill them.
        // Sizing within a group keeps alignment clean where the eye
        // actually scans (issue #961).
        let mut rendered_workspace_lines =
            self.prebuild_workspace_lines(row_budget, theme, now, focused);

        let lines: Vec<Line> = self
            .visible
            .iter()
            .enumerate()
            .map(|(i, row)| match row {
                VisibleRow::FocusedHeader => {
                    use crate::components::icons;
                    let is_cursor = i == self.cursor;
                    let row_bg = if is_cursor && focused {
                        Some(theme.row_focused())
                    } else if is_cursor {
                        Some(theme.row_unfocused())
                    } else {
                        None
                    };
                    // Synthetic top group. No disclosure glyph (it never
                    // collapses) — a star in the gutter, then the label,
                    // both in the accent tone so the shortlist reads as a
                    // deliberate pin above the algorithmic repo groups.
                    // Honor `display.ascii_glyphs` like every other row
                    // glyph so a non-Nerd-Font terminal gets a plain `*`.
                    let star = if self.ascii_glyphs { "*" } else { icons::STAR };
                    let mut spans: Vec<Span> = vec![
                        Span::styled(
                            format!("{star} "),
                            row_bg.unwrap_or_default().fg(theme.accent),
                        ),
                        Span::styled(
                            "Focused",
                            row_bg
                                .unwrap_or_default()
                                .fg(theme.accent)
                                .add_modifier(Modifier::BOLD),
                        ),
                    ];
                    if let Some(bg) = row_bg {
                        extend_cursor_fill(&mut spans, row_budget, bg);
                    }
                    Line::from(spans)
                }
                VisibleRow::SpaceHeader(name) => {
                    // Top tier of the tree (#860): sits above the repo
                    // headers it contains, styled with the accent colour
                    // and the folder glyph so the eye reads it as a level
                    // higher than the warn-coloured repo rows beneath.
                    use crate::components::icons;
                    let collapsed = self.collapsed_spaces.contains(name);
                    let glyph = if collapsed { "▸" } else { "▾" };
                    let is_cursor = i == self.cursor;
                    let row_bg = if is_cursor && focused {
                        Some(theme.row_focused())
                    } else if is_cursor {
                        Some(theme.row_unfocused())
                    } else {
                        None
                    };
                    let glyph_style = match row_bg {
                        Some(bg) => bg,
                        None => Style::default().fg(theme.text_dim),
                    };
                    let mut spans: Vec<Span> = vec![
                        Span::styled(format!("{glyph} "), glyph_style),
                        Span::styled(
                            format!("{} {}", icons::SPACE, name),
                            row_bg
                                .unwrap_or_default()
                                .fg(theme.accent)
                                .add_modifier(Modifier::BOLD),
                        ),
                    ];
                    if let Some(bg) = row_bg {
                        extend_cursor_fill(&mut spans, row_budget, bg);
                    }
                    Line::from(spans)
                }
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
                    // A pin marker on a pinned group — the visual
                    // affordance for the "float to top" order (#760).
                    // Read the field directly rather than via
                    // `is_repo_pinned`: this closure already borrows
                    // `self.visible`, and a `&self` method call would
                    // extend that to a whole-`self` borrow that clashes
                    // with the `self.scroll` writes below (same reason
                    // the collapsed-glyph check above reads its field
                    // directly).
                    if self.pinned_repos.iter().any(|r| r == name) {
                        spans.push(Span::styled(
                            format!(" {}", icons::PIN),
                            row_bg.unwrap_or_default().fg(theme.accent),
                        ));
                    }
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
                        // While a search filters this project — either a
                        // scoped `/` on it or a global `#` search that
                        // touches every repo — surface the match count so
                        // the header reads as a result tally rather than a
                        // vanished tree.
                        if self.search.as_ref().is_some_and(|q| {
                            !q.query.is_empty()
                                && q.scope.as_deref().is_none_or(|scope| scope == name)
                        }) {
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

        // Bottom search bar (fzf-style). A scoped `/` search leads with
        // `/`; a global search leads with `⌕` (matching the header box)
        // and names `all repos` so the wider reach is obvious. While
        // editing a caret trails the query; once `Enter` keeps the
        // filter applied the caret drops and a hint reminds the user
        // how to re-edit and clear.
        self.search_bar_rect = None;
        if search_bar && let Some(s) = self.search.as_ref() {
            let bar = Rect::new(area.x + l_pad, area.y + area.height - 1, inner_width, 1);
            self.search_bar_rect = Some(bar);
            let global = s.scope.is_none();
            let prefix = if global { "⌕ " } else { "/ " };
            let mut spans = vec![
                Span::styled(prefix, Style::default().fg(theme.accent)),
                Span::styled(s.query.clone(), Style::default().fg(theme.text_strong)),
            ];
            if s.editing {
                spans.push(Span::styled("▏", Style::default().fg(theme.accent)));
            }
            // Trailing guidance for a scoped `/` search. The actionable
            // cues lead so they survive a narrow-pane clip: the `#`
            // pointer to the wider reach, then `esc clear`, with the
            // project NAME last — it's the most expendable (the repo
            // header already shows it, and `/` is inherently on the
            // focused project). When the scope matches nothing the `#`
            // pointer is the whole point, so the empty hint drops the
            // (clip-prone) scope name entirely and reads as "look wider"
            // rather than "broken" (#1033). `esc clear` holds in every
            // state — Esc clears both an editing and a committed search.
            let empty_scoped = !s.query.is_empty()
                && s.scope.as_deref().is_some_and(|scope| {
                    self.repo_summaries.get(scope).map_or(0, |sm| sm.active) == 0
                });
            let hint = match (&s.scope, empty_scoped) {
                (Some(_), true) => "  no matches · # all repos · esc clear".to_string(),
                (Some(scope), false) => format!("  # all repos · esc clear · {scope}"),
                (None, _) => "  all repos · esc clear".to_string(),
            };
            spans.push(Span::styled(hint, Style::default().fg(theme.text_dim)));
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

    /// Build & lay out every visible workspace row and scatter the
    /// resulting Lines back to the visible-list indices they belong to.
    ///
    /// The returned `Vec<Option<Line>>` has `self.visible.len()`
    /// slots, with `Some(line)` at every `VisibleRow::Workspace`
    /// position and `None` everywhere else. The caller `.take()`s
    /// each Line as it walks `self.visible`, so every Line is moved
    /// exactly once.
    ///
    /// Rows are laid out **per group** — one `render_table` pass per
    /// repo / provider group rather than one global pass. Each group's
    /// pass fixes issue #22's column-drift *within* the group (each
    /// `Max` column picks one width across the group's rows instead of
    /// collapsing per-row on an empty cell), while keeping providers
    /// with different columns from distorting one another (issue #961):
    /// GitHub's `#NNN` and CI/review columns size only over GitHub
    /// groups; a Linear group reserves neither.
    fn prebuild_workspace_lines(
        &self,
        row_budget: usize,
        theme: &crate::theme::Theme,
        now: chrono::DateTime<chrono::Utc>,
        focused: bool,
    ) -> Vec<Option<Line<'static>>> {
        // 1-based jump numbers for the first nine focused workspaces, in
        // sidebar order — the badge that pairs with the `]]<digit>`
        // jump. Past the ninth there's no single-digit key, so it gets
        // no badge.
        let agent_numbers: std::collections::HashMap<SessionKey, usize> = self
            .numbered_workspace_keys()
            .into_iter()
            .take(9)
            .enumerate()
            .map(|(i, k)| (k, i + 1))
            .collect();
        // Aggregate runner badges + agent-model labels for every
        // workspace once, so each row is an O(1) lookup instead of
        // re-scanning all running terminals per row (#1031).
        let badges_by_key = self.runner_badges_by_key();
        let models_by_key = self.agent_models_by_key();
        let mut out: Vec<Option<Line<'static>>> = vec![None; self.visible.len()];
        // Partition workspace rows into groups delimited by their
        // top-level container header — a repo group, or the synthetic
        // Focused pin. A `KindHeader` (PRs vs Issues) doesn't split the
        // group: it stays within one repo, so one provider. Session
        // sub-rows are skipped and don't break the run.
        let mut group: Vec<usize> = Vec::new();
        for (i, row) in self.visible.iter().enumerate() {
            match row {
                VisibleRow::FocusedHeader | VisibleRow::RepoHeader(_) => {
                    self.render_workspace_group(
                        &group,
                        &agent_numbers,
                        &badges_by_key,
                        &models_by_key,
                        row_budget,
                        theme,
                        now,
                        focused,
                        &mut out,
                    );
                    group.clear();
                }
                VisibleRow::Workspace(_) => group.push(i),
                _ => {}
            }
        }
        self.render_workspace_group(
            &group,
            &agent_numbers,
            &badges_by_key,
            &models_by_key,
            row_budget,
            theme,
            now,
            focused,
            &mut out,
        );
        out
    }

    /// Lay out one group of workspace rows (given by their visible-list
    /// indices), sizing columns per provider. A repo group is a single
    /// provider (one pass), but the `★ Focused` pin lifts starred rows
    /// across providers into one group — GitHub's `#NNN` / CI / review
    /// columns must not size against a Linear row there either, so the
    /// group is sub-partitioned by provider before the column pre-pass
    /// (issue #961).
    #[allow(clippy::too_many_arguments)]
    fn render_workspace_group(
        &self,
        indices: &[usize],
        agent_numbers: &std::collections::HashMap<SessionKey, usize>,
        badges_by_key: &std::collections::HashMap<SessionKey, Vec<(char, usize)>>,
        models_by_key: &std::collections::HashMap<SessionKey, Vec<(char, String)>>,
        row_budget: usize,
        theme: &crate::theme::Theme,
        now: chrono::DateTime<chrono::Utc>,
        focused: bool,
        out: &mut [Option<Line<'static>>],
    ) {
        // Bucket the group's rows by provider (`task.id.source`), keeping
        // first-seen order; a task-less scratch row keys as `None`. Each
        // bucket gets its own column pass so unlike providers can't
        // distort one another's reference / status columns.
        let mut buckets: Vec<(Option<&str>, Vec<usize>)> = Vec::new();
        for &i in indices {
            let provider = match &self.visible[i] {
                VisibleRow::Workspace(k) => self
                    .workspaces
                    .get(k)
                    .and_then(|w| w.primary_task())
                    .map(|t| t.id.source.as_str()),
                _ => None,
            };
            match buckets.iter_mut().find(|(p, _)| *p == provider) {
                Some((_, rows)) => rows.push(i),
                None => buckets.push((provider, vec![i])),
            }
        }
        for (_, bucket) in &buckets {
            self.render_provider_bucket(
                bucket,
                agent_numbers,
                badges_by_key,
                models_by_key,
                row_budget,
                theme,
                now,
                focused,
                out,
            );
        }
    }

    /// Lay out one single-provider bucket of workspace rows in a single
    /// `render_table` pass, sizing the reference and status columns over
    /// only these rows, and write each Line into its slot in `out`.
    #[allow(clippy::too_many_arguments)]
    fn render_provider_bucket(
        &self,
        indices: &[usize],
        agent_numbers: &std::collections::HashMap<SessionKey, usize>,
        badges_by_key: &std::collections::HashMap<SessionKey, Vec<(char, usize)>>,
        models_by_key: &std::collections::HashMap<SessionKey, Vec<(char, String)>>,
        row_budget: usize,
        theme: &crate::theme::Theme,
        now: chrono::DateTime<chrono::Utc>,
        focused: bool,
        out: &mut [Option<Line<'static>>],
    ) {
        if indices.is_empty() {
            return;
        }
        // Widest `NNN` within this bucket so every row in it pads to the
        // same reference column. Width is the digit count of `n` (no `#`
        // prefix — issue #67; the type glyph carries the issue-vs-PR
        // signal), via `ilog10` so the hot path doesn't allocate. The
        // column is Fixed and LEFT-aligned, so the deficit pads on the
        // right and the number stays flush off the glyph on every row
        // (issues #42/#65). Scoping the max to the bucket is #961: a
        // wide `#NNN` in another repo/provider no longer inflates this one.
        let max_pr_num_width = indices
            .iter()
            .filter_map(|&i| match &self.visible[i] {
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
        let columns = crate::components::workspace_row::build_columns(max_pr_num_width);
        // Track the source index for each built row so the rendered Lines
        // scatter back to the exact visible positions they came from —
        // never a positional `zip` against `indices`, which would silently
        // misalign every following row if one index were ever skipped.
        let mut row_indices: Vec<usize> = Vec::with_capacity(indices.len());
        let mut rows: Vec<TableRow> = Vec::with_capacity(indices.len());
        for &i in indices {
            let VisibleRow::Workspace(key) = &self.visible[i] else {
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
                limit_reached: workspace.is_some_and(|w| {
                    crate::agent_attention::workspace_is_limit_reached(w, &self.agents)
                }),
                working: workspace
                    .is_some_and(|w| crate::agent_attention::workspace_is_working(w, &self.agents)),
                done: workspace
                    .is_some_and(|w| crate::agent_attention::workspace_is_done(w, &self.agents)),
                exited: workspace
                    .is_some_and(|w| crate::agent_attention::workspace_is_exited(w, &self.agents)),
                working_glyph: crate::components::workspace_row::working_glyph(
                    self.working_spinner_frame,
                ),
                spawning: self.is_spawning(key),
                spawning_glyph: crate::components::workspace_row::spawning_glyph(
                    self.working_spinner_frame,
                ),
                badges: badges_by_key.get(key).cloned().unwrap_or_default(),
                agent_models: models_by_key.get(key).cloned().unwrap_or_default(),
                agent_number: agent_numbers.get(key).copied(),
                ascii_glyphs: self.ascii_glyphs,
                auto_merge_armed: workspace.is_some_and(|w| w.auto_merge_on_green),
                auto_merge_enabled: workspace
                    .and_then(|w| w.primary_task())
                    .is_some_and(|t| t.auto_merge_enabled),
                auto_fix_ci_armed: workspace.is_some_and(|w| {
                    w.policies.arm(lazybox_core::AutoFixKind::CiFailure)
                        == lazybox_core::PolicyArm::Arm
                }),
                auto_fix_conflict_armed: workspace.is_some_and(|w| {
                    w.policies.arm(lazybox_core::AutoFixKind::MergeConflict)
                        == lazybox_core::PolicyArm::Arm
                }),
                track_main: workspace.is_some_and(|w| w.track_main),
                track_main_behind: workspace.is_some_and(|w| w.track_main && w.track_main_behind),
                has_notes: workspace.is_some_and(|w| w.has_notes()),
                sent_snippet_count: workspace.map_or(0, |w| w.sent_snippets.len()),
                stack: self.stacks.get(key),
                model_shorts: &self.model_shorts,
            };
            row_indices.push(i);
            rows.push(build_workspace_row(&ctx));
        }
        let lines = crate::components::table::render_table(&rows, &columns, row_budget);
        for (i, line) in row_indices.into_iter().zip(lines) {
            out[i] = Some(line);
        }
    }
}
