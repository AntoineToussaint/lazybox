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
                    // The "can I keep working?" headroom figure — accented
                    // (bold accent) so it reads as its own signal next to the
                    // dim cost meta.
                    UsageSpanKind::Quota => Style::default()
                        .fg(theme.accent)
                        .add_modifier(Modifier::BOLD),
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

/// USD micros (millionths of a dollar) → `$1.23`.
fn fmt_cost_micros(micros: i64) -> String {
    format!("${:.2}", micros as f64 / 1_000_000.0)
}

/// Style + width-gate the always-visible "today" stats strip (#1344): a
/// terse `today  3 sessions · 4 merged · $2.14` of the persisted daily
/// rollup. Like [`usage_line_spans`], whole groups drop rather than clip
/// when the sidebar is narrow — priority high→low is sessions, merged,
/// cost, so a tight header keeps the headline accomplishment count and
/// sheds cost first. Empty when no group fits, which the caller reads as
/// "no today row" (so the bare `today` label never renders alone).
fn today_line_spans(
    stats: &TodayStats,
    inner_width: usize,
    theme: &crate::theme::Theme,
) -> Vec<Span<'static>> {
    let dim = Style::default().fg(theme.text_dim);
    let figure = Style::default().add_modifier(Modifier::BOLD);
    let groups: [Vec<Span<'static>>; 3] = [
        vec![
            Span::styled(stats.sessions.to_string(), figure),
            Span::styled(" sessions", dim),
        ],
        vec![
            Span::styled(stats.merged.to_string(), figure),
            Span::styled(" merged", dim),
        ],
        vec![Span::styled(fmt_cost_micros(stats.cost_micros), figure)],
    ];
    let mut out: Vec<Span<'static>> = vec![Span::styled("today", dim)];
    let mut used = spans_visual_width(&out);
    let mut any = false;
    for group in groups {
        // Two spaces after the label so it reads as a heading; a dim
        // ` · ` between groups.
        let sep = if any { 3 } else { 2 };
        let group_width = spans_visual_width(&group);
        // The groups are priority-ordered, so stop at the first that
        // doesn't fit rather than skipping it for a narrower lower-priority
        // one — a tight header sheds cost before merged before sessions,
        // never the reverse (unlike the equal-priority `usage_line_spans`).
        if used + sep + group_width > inner_width {
            break;
        }
        out.push(if any {
            Span::styled(" · ", dim)
        } else {
            Span::raw("  ")
        });
        out.extend(group);
        used += sep + group_width;
        any = true;
    }
    if any { out } else { Vec::new() }
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

/// The header's attention tallies, computed in one pass and memoized
/// (2026-08-19 audit, U4). Mirrors `workspace_count` /
/// `total_unread_count` / `count_visible_with_signal` /
/// `visible_broadcast_selected_count` / `limit_reached_workspace_count`
/// exactly — those remain the semantic source of truth for the jumps
/// and tips that run off the render path.
#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
pub(crate) struct HeaderCounters {
    pub(crate) count: usize,
    pub(crate) unread: usize,
    pub(crate) input_pending: usize,
    pub(crate) ci_failing: usize,
    pub(crate) review_pending: usize,
    pub(crate) selected: usize,
    pub(crate) limited: usize,
}

/// One-shot `DefaultHasher` over a single `Hash` value — a building block
/// for the render-line cache signature (#1090).
fn hash_one<H: std::hash::Hash>(v: &H) -> u64 {
    use std::hash::Hasher;
    let mut h = std::collections::hash_map::DefaultHasher::new();
    v.hash(&mut h);
    h.finish()
}

/// Collapse an `AgentState` to a stable code for the signature. `AgentState`
/// isn't `Hash` (and lives in `ipc`, where adding a derive would churn the
/// wire fixture), so we fold it here — every variant that changes a row's
/// glyph must map to a distinct value.
fn agent_state_code(s: &lazybox_ipc::AgentState) -> u64 {
    use lazybox_ipc::AgentState::*;
    match s {
        Working => 1,
        InputNeeded => 2,
        Idle => 3,
        Done => 4,
        Exited { code } => 5u64 ^ hash_one(code).rotate_left(8),
        LimitReached => 6,
        CreditExhausted => 7,
    }
}

/// Same for `TerminalKind` — the variant (and the agent id) decides a row's
/// runner badge, so it must move the signature.
fn terminal_kind_code(k: &lazybox_ipc::TerminalKind) -> u64 {
    use lazybox_ipc::TerminalKind::*;
    match k {
        Agent(id) => 0xA000 ^ hash_one(id),
        Shell => 0x5000,
        LogTail { path } => 0x1000 ^ hash_one(path),
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

    /// The "today" strip's spans, or empty when the row is disabled
    /// (`ui.today_summary` off), no snapshot has landed yet, or nothing
    /// fits `inner_width`. Mirrors `usage_summaries` as the single
    /// gate the render + hit-test both read. Today's slice is re-summed
    /// against the current calendar day here, not cached, so the row
    /// rolls over at local midnight (#1344).
    fn today_spans(&self, inner_width: usize, theme: &crate::theme::Theme) -> Vec<Span<'static>> {
        if !self.today_summary {
            return Vec::new();
        }
        let Some(buckets) = &self.today_buckets else {
            return Vec::new();
        };
        let stats = TodayStats::from_buckets(buckets, self.local_today());
        today_line_spans(&stats, inner_width, theme)
    }

    /// Height the always-visible "today" strip adds to the header for a
    /// pane of this width — `1` when it renders, `0` otherwise (#1344).
    /// The click hit-tests fold this into their header offset, alongside
    /// the usage row, so a click still lands on the row actually drawn.
    pub(super) fn today_row_height(&self, area: Rect) -> u16 {
        let inner_width = area.width.saturating_sub(2) as usize;
        let theme = crate::theme::current();
        if self.today_spans(inner_width, theme).is_empty() {
            0
        } else {
            1
        }
    }

    /// Total header rows above the content: the fixed 5 (brand, filter,
    /// stats, divider, blank) plus the two optional strips. The click
    /// hit-tests read this so a click resolves to the row actually drawn
    /// once the usage / today rows shift content down.
    pub(super) fn header_height(&self, area: Rect) -> u16 {
        5 + self.usage_row_height(area) + self.today_row_height(area)
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
        // One memoized pass for every header tally (U4) — the
        // individual count fns remain the single source of semantics
        // and are what `compute_header_counters` mirrors; jumps/tips
        // still call them directly off the render path.
        let counters = self.header_counters();
        let count = counters.count;
        let unread = counters.unread;
        let input_pending = counters.input_pending;
        let ci_failing = counters.ci_failing;
        let review_pending = counters.review_pending;

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
        let selected = counters.selected;
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
        let limited = counters.limited;
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

        // Row 1 — filter + sort + search chips. Each is a plain
        // key-plus-label hint (`f filter`, `o recent`, `# find`), dim
        // while at its default and accent-bold once a non-default value
        // is set, so the row stays quiet by default but shouts when a
        // filter is on. The chips are still clickable, but deliberately
        // rendered as labels, not bracketed buttons: the `[…]` framing
        // read as a clickable button and invited the "type a word and
        // fire a burst of shortcuts" failure the header is meant to
        // avoid (#1110). The filter chip lists every active filter
        // (comma-joined); clicking it opens the filter menu.
        if area.height >= 2 {
            let row1 = Rect::new(area.x + l_pad, area.y + 1, inner_width, 1);

            let filter_active = !self.filters.is_empty();
            let sort_active = self.sort_mode != SortMode::Recent;
            let active_style = Style::default()
                .fg(theme.accent)
                .add_modifier(Modifier::BOLD);
            let dim_style = Style::default().fg(theme.text_dim);
            // The shortcut keys are hints, not buttons — kept dim so the
            // row never reads as a strip of pressable controls (#1110).
            let key_style = dim_style;

            let filter_prefix = "f ";
            // Collapse past two chips to `a, b, +N` so a wide active
            // set can't push the sort chip off the row or truncate a
            // label mid-word — the full set is one `f` away.
            let filter_chip = if filter_active {
                let chips = self.filters.chips();
                if chips.len() <= 2 {
                    chips.join(", ")
                } else {
                    format!("{}, +{}", chips[..2].join(", "), chips.len() - 2)
                }
            } else {
                "filter".to_string()
            };
            let sep = "  ";
            let sort_prefix = "o ";
            let sort_chip = self.sort_mode.chip_label().to_string();

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
            // The `⌕ …` frame around the query; its width is measured
            // rather than assumed so a wider search glyph can't push the
            // chip past the room the guard below reserved for it.
            const SEARCH_FRAME: &str = "⌕ ";
            let search_chip = match global_query {
                Some(q) => {
                    let q_room =
                        chip_room.saturating_sub(visual_width(SEARCH_FRAME) as u16) as usize;
                    format!("⌕ {}", truncate_ellipsis(q, q_room.max(1)))
                }
                None => "find".to_string(),
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
        // Metering canary: the focused workspace is routed through the
        // metering proxy (`$ meter`), so show it plus its accrued per-session
        // cost the moment any priced usage lands. `$ METER` alone until the
        // first response is priced (proxy off / unknown model → no cost).
        let focused_meter = focused_workspace.and_then(|workspace| {
            if !workspace.metered {
                return None;
            }
            let cost = self.usage.cost_micros_for_session(workspace.key.as_str());
            Some(if cost > 0 {
                format!(
                    " $ METER · {} ",
                    lazybox_tui_core::usage::format_cost_micros(cost)
                )
            } else {
                " $ METER ".to_string()
            })
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
        if let Some(label) = focused_meter {
            try_append(
                &mut stats_spans,
                &mut used,
                budget,
                vec![Span::styled(
                    label,
                    Style::default()
                        .bg(theme.accent)
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

        // Row 4 (when present) — the always-visible "today" stats strip
        // (#1344). A terse `today  3 sessions · 4 merged · $2.14` of the
        // persisted daily rollup (#1339), width-gated the same way as the
        // usage row: a group that can't fit whole is dropped rather than
        // sliced. Sits just below the usage row (or row 2 when there is
        // none) and above the divider, so it reads as header chrome; absent
        // (`ui.today_summary` off, or no snapshot yet) it takes no space.
        let today_spans = self.today_spans(inner_width as usize, theme);
        let today_h: u16 = if today_spans.is_empty() { 0 } else { 1 };
        if today_h == 1 && area.height >= 4 + usage_h {
            let today_area = Rect::new(area.x + l_pad, area.y + 3 + usage_h, inner_width, 1);
            frame.render_widget(Paragraph::new(Line::from(today_spans)), today_area);
        }

        // Divider — thin, accent-tinted while this pane has focus so the
        // active pane reads at a glance (#286). Sits just under the usage /
        // today rows (or row 2 when there is neither).
        let divider_y = 3 + usage_h + today_h;
        if area.height >= 4 + usage_h + today_h {
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
        let header_height: u16 = 5 + usage_h + today_h;
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

        // Clamp the scroll BEFORE building any rows (2026-08-19 audit,
        // U5), then build ONLY the viewport window. Every `VisibleRow`
        // is exactly one line, so the window is a plain range over
        // `visible` — the old path built and laid out every row of the
        // whole list each frame (~150 rows against a ~40-row viewport)
        // and let the Paragraph widget skip the off-screen prefix.
        let total_rows = self.visible.len();
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

        // Workspace rows are laid out per group, not globally: each repo /
        // provider group runs its own column pre-pass over just its own
        // rows. GitHub's `#NNN` and Linear's `TEAM-NNNN` are different
        // shapes, and their status columns differ (GitHub has CI / review,
        // Linear doesn't) — a single global layout let one provider's
        // widest reference bloat the other's column and reserved
        // provider-specific status columns for groups that never fill them.
        // Sizing within a group keeps alignment clean where the eye
        // actually scans (issue #961). Only groups intersecting the
        // viewport window are built at all: the cursor is part of the
        // line-cache signature, so every j/k used to rebuild the ENTIRE
        // list — O(total workspaces) per keystroke on a crowded sidebar.
        // A partially-visible group still lays out ALL its rows, so
        // column widths stay stable while it scrolls (#22 / #961).
        let window = self.scroll..self.scroll + viewport;
        let rendered_workspace_lines =
            self.cached_workspace_lines(row_budget, theme, now, focused, window);

        let lines: Vec<Line> = self
            .visible
            .iter()
            .enumerate()
            .skip(self.scroll)
            .take(viewport)
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
                VisibleRow::HopperHeader => {
                    use crate::components::icons;
                    let is_cursor = i == self.cursor;
                    let row_bg = if is_cursor && focused {
                        Some(theme.row_focused())
                    } else if is_cursor {
                        Some(theme.row_unfocused())
                    } else {
                        None
                    };
                    let count = self
                        .workspaces
                        .values()
                        .filter(|workspace| workspace.hopper.is_some())
                        .count();
                    let glyph = if self.ascii_glyphs {
                        ">"
                    } else {
                        icons::HOPPER
                    };
                    let mut spans = vec![
                        Span::styled(
                            format!("{glyph} "),
                            row_bg.unwrap_or_default().fg(theme.text_dim),
                        ),
                        Span::styled(
                            "Hopper",
                            row_bg
                                .unwrap_or_default()
                                .fg(theme.accent)
                                .add_modifier(Modifier::BOLD),
                        ),
                        Span::styled(
                            format!("  {count}"),
                            row_bg.unwrap_or_default().fg(theme.text_dim),
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
                    // Space-tier metering badge (approach C): a `$` marks every
                    // workspace under this Space as metered (`x $` toggles it).
                    if self.metered_spaces.contains(name) {
                        spans.push(Span::styled(
                            " $",
                            row_bg.unwrap_or_default().fg(theme.accent),
                        ));
                    }
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
                    // Source-attention ladder (#scale): a demoted (quiet /
                    // digest / muted) group's header drops the loud
                    // bold-warn styling — dim, so a wall of muted repos
                    // reads as background, not work.
                    let demoted = self
                        .repo_summaries
                        .get(name)
                        .and_then(|s| s.source_attention.as_deref())
                        .is_some();
                    let name_style = if demoted {
                        row_bg
                            .unwrap_or_default()
                            .fg(theme.text_dim)
                            .add_modifier(Modifier::DIM)
                    } else {
                        row_bg
                            .unwrap_or_default()
                            .fg(theme.warn)
                            .add_modifier(Modifier::BOLD)
                    };
                    let mut spans: Vec<Span> = vec![
                        Span::styled(format!("{glyph} "), glyph_style),
                        Span::styled(format!("{} {}", icons::REPO, name), name_style),
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
                        // Attention-ladder chip (#scale): name the
                        // level, the wake time for a time-boxed source
                        // snooze, and — for a collapsed muted group —
                        // the count of rows folded behind the residue
                        // header (muted-but-counted, never invisible).
                        if let Some(level) = s.source_attention.as_deref() {
                            let mut chip = match s.source_snooze_until_epoch_ms {
                                Some(ms) => {
                                    let until = chrono::DateTime::from_timestamp_millis(ms)
                                        .unwrap_or_else(chrono::Utc::now);
                                    format!(
                                        "  ⏾ wakes {}",
                                        crate::components::sidebar::relative_time(now, until)
                                    )
                                }
                                None => format!("  ⌀ {level}"),
                            };
                            if collapsed && s.active > 0 {
                                chip.push_str(&format!(" · {}", s.active));
                            }
                            spans.push(Span::styled(
                                chip,
                                row_bg.unwrap_or_default().fg(theme.text_dim),
                            ));
                        }
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
                // Clone from the shared cache — only the viewport's
                // workspace rows pay it, not all ~150 (U2/U5).
                VisibleRow::Workspace(_) => rendered_workspace_lines
                    .get(i)
                    .and_then(|slot| slot.clone())
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

        // The window above already starts at `self.scroll` (clamped
        // pre-build; a wheel-detached viewport keeps its position, and
        // explicit cursor moves clear the detach flag so j/k still
        // re-anchor) — the Paragraph paints the slice from its top.
        let para = Paragraph::new(lines);
        frame.render_widget(para, inner);

        // First-run / empty-inbox guidance. When the list is genuinely
        // empty (default Inbox, no filter, no search) the blank pane is
        // a dead end — replace it with a panel that names the next
        // actions, foregrounding the worktree-session flow that works
        // with zero GitHub data (issue #100).
        if self.is_getting_started() {
            self.render_getting_started(inner, frame, theme);
        } else if let Some(query) = self.search.as_ref().and_then(|s| {
            let q = s.query.trim();
            (!q.is_empty() && self.workspace_count() == 0).then(|| q.to_string())
        }) {
            // A live search that filtered every workspace away. A blank
            // pane reads as "everything vanished / broke"; this names the
            // query and the way out instead (#1099).
            self.render_no_matches(inner, frame, theme, &query);
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
            // While the bar is capturing keystrokes, fill it with a solid
            // block so it reads unmistakably as a search field — not just
            // another list row you can keep typing "into" (#1099). The
            // `🔍` glyph + `/` prefix give it the vim `/pattern` shape.
            let field = s.editing;
            let with_field_bg = |st: Style| if field { st.bg(theme.fill) } else { st };
            if field {
                frame.render_widget(Block::default().style(Style::default().bg(theme.fill)), bar);
            }
            let prefix = if global { "🔍 ⌕ " } else { "🔍 /" };
            let mut spans = vec![
                Span::styled(
                    prefix,
                    with_field_bg(
                        Style::default()
                            .fg(theme.accent)
                            .add_modifier(Modifier::BOLD),
                    ),
                ),
                Span::styled(
                    s.query.clone(),
                    with_field_bg(
                        Style::default()
                            .fg(theme.text_strong)
                            .add_modifier(Modifier::BOLD),
                    ),
                ),
            ];
            if field {
                // A solid block cursor — a visible caret so it's obvious
                // where typed characters land (#1099).
                spans.push(Span::styled(
                    "█",
                    with_field_bg(Style::default().fg(theme.accent)),
                ));
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
            spans.push(Span::styled(
                hint,
                with_field_bg(Style::default().fg(theme.text_dim)),
            ));
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

    /// Paint the empty-search panel: a live query filtered every row away,
    /// so instead of a blank pane (which reads as "everything vanished")
    /// name the query and the way out (#1099).
    fn render_no_matches(
        &self,
        inner: Rect,
        frame: &mut Frame,
        theme: &crate::theme::Theme,
        query: &str,
    ) {
        if inner.height < 2 {
            return;
        }
        let heading = Style::default().fg(theme.warn).add_modifier(Modifier::BOLD);
        let dim = Style::default().fg(theme.text_dim);
        // Clip a pathological query so the message can't itself overflow.
        let shown = truncate_ellipsis(query, inner.width.saturating_sub(20).max(8) as usize);
        let lines: Vec<Line<'static>> = vec![
            Line::raw(""),
            Line::from(Span::styled(format!(" No matches for “{shown}”"), heading)),
            Line::raw(""),
            Line::from(Span::styled(" Esc to clear the search", dim)),
            Line::from(Span::styled(" # searches all repos", dim)),
        ];
        frame.render_widget(Paragraph::new(lines), inner);
    }

    /// A signature over everything `prebuild_workspace_lines` reads, so the
    /// built lines can be memoized across frames (#1090). Workspace *content*
    /// (titles, CI, reviews, labels, unread, policies, …) is captured wholesale
    /// by `data_version`, which bumps on every `recompute_visible_inner` — and
    /// all of that content reaches the sidebar as a daemon upsert that
    /// recomputes. Everything the render reads that *doesn't* go through a
    /// recompute — the cursor, per-agent state, the spinner frame, the runner /
    /// model / spawning / selection / stack maps, theme, search, width — is
    /// folded in explicitly here. Maps are folded order-independently
    /// (`wrapping_add` of per-entry hashes) since their iteration order is not
    /// stable. A missed input can only cost one frame of cosmetic lag that the
    /// next redraw heals; it can never desync dispatch, which reads live state.
    /// All header-badge tallies computed in ONE pass over the visible
    /// list (2026-08-19 audit, U4) — see `header_counters`.
    fn compute_header_counters(&self) -> HeaderCounters {
        use lazybox_tui_core::inbox::AttentionSignal;
        let mut c = HeaderCounters::default();
        for row in &self.visible {
            let VisibleRow::Workspace(key) = row else {
                continue;
            };
            let Some(workspace) = self.workspaces.get(key) else {
                continue;
            };
            c.count += 1;
            c.unread += workspace.unread_count();
            let signals = workspace_attention_signals(workspace, &self.agents);
            if signals.contains(&AttentionSignal::AgentAsking) {
                c.input_pending += 1;
            }
            if signals.contains(&AttentionSignal::CiFailing) {
                c.ci_failing += 1;
            }
            if signals.contains(&AttentionSignal::ReviewPending) {
                c.review_pending += 1;
            }
            if self.broadcast_selected.contains(key) {
                c.selected += 1;
            }
        }
        c.limited = self.limit_reached_workspace_count();
        c
    }

    /// Memoized front door to [`Self::compute_header_counters`]. The
    /// header used to re-run 7 independent O(visible × activity) scans
    /// per frame; the counters can only change when the workspace data
    /// recomputes (`data_version`), an agent state moves, or the
    /// multi-select changes — all folded into the key.
    pub(crate) fn header_counters(&mut self) -> HeaderCounters {
        use std::hash::{Hash, Hasher};
        let mut h = std::collections::hash_map::DefaultHasher::new();
        self.data_version.hash(&mut h);
        let mut fold: u64 = 0;
        for (k, st) in &self.agents {
            fold = fold.wrapping_add(hash_one(k) ^ agent_state_code(st).rotate_left(1));
        }
        fold.hash(&mut h);
        let mut fold: u64 = 0;
        for (tid, (sk, st)) in &self.agent_terminal_states {
            fold = fold
                .wrapping_add(hash_one(tid) ^ hash_one(sk).rotate_left(3) ^ agent_state_code(st));
        }
        fold.hash(&mut h);
        let mut fold: u64 = 0;
        for k in &self.broadcast_selected {
            fold = fold.wrapping_add(hash_one(k).rotate_left(7));
        }
        fold.hash(&mut h);
        let sig = h.finish();
        if let Some((cached_sig, counters)) = self.header_counters_cache
            && cached_sig == sig
        {
            return counters;
        }
        let counters = self.compute_header_counters();
        self.header_counters_cache = Some((sig, counters));
        counters
    }

    fn workspace_lines_signature(
        &self,
        row_budget: usize,
        focused: bool,
        now: chrono::DateTime<chrono::Utc>,
        window: &std::ops::Range<usize>,
    ) -> u64 {
        use std::hash::{Hash, Hasher};
        let mut h = std::collections::hash_map::DefaultHasher::new();
        self.data_version.hash(&mut h);
        self.cursor.hash(&mut h);
        // Only viewport-intersecting groups are built, so the cached
        // lines are only valid for the window they were built against.
        window.start.hash(&mut h);
        window.end.hash(&mut h);
        self.working_spinner_frame.hash(&mut h);
        // Relative timestamps ("2h", "3d") only shift at minute granularity,
        // so quantize — otherwise every frame is a fresh second and the cache
        // never hits.
        (now.timestamp() / 60).hash(&mut h);
        row_budget.hash(&mut h);
        focused.hash(&mut h);
        self.ascii_glyphs.hash(&mut h);
        self.show_agent_model.hash(&mut h);
        crate::theme::current().name.hash(&mut h);
        match &self.search {
            Some(s) => {
                1u8.hash(&mut h);
                s.query.hash(&mut h);
            }
            None => 0u8.hash(&mut h),
        }
        let mut fold: u64 = 0;
        for (k, st) in &self.agents {
            fold = fold.wrapping_add(hash_one(k) ^ agent_state_code(st).rotate_left(1));
        }
        fold.hash(&mut h);
        let mut fold: u64 = 0;
        for (tid, (sk, kind)) in &self.running_terminals {
            fold = fold.wrapping_add(
                hash_one(tid) ^ hash_one(sk).rotate_left(3) ^ terminal_kind_code(kind),
            );
        }
        fold.hash(&mut h);
        let mut fold: u64 = 0;
        for (tid, model) in &self.terminal_models {
            fold = fold.wrapping_add(hash_one(tid) ^ hash_one(model).rotate_left(5));
        }
        fold.hash(&mut h);
        let mut fold: u64 = 0;
        for k in &self.spawning {
            fold = fold.wrapping_add(hash_one(k));
        }
        fold.hash(&mut h);
        let mut fold: u64 = 0;
        for k in &self.broadcast_selected {
            fold = fold.wrapping_add(hash_one(k).rotate_left(7));
        }
        fold.hash(&mut h);
        let mut fold: u64 = 0;
        for (k, sp) in &self.stacks {
            let mut e = hash_one(k);
            e ^= (sp.position as u64).rotate_left(3);
            e ^= (sp.depth as u64).rotate_left(11);
            e ^= (sp.children.len() as u64).rotate_left(17);
            e ^= hash_one(&sp.parent).rotate_left(23);
            fold = fold.wrapping_add(e);
        }
        fold.hash(&mut h);
        let mut fold: u64 = 0;
        for (k, v) in &self.model_shorts {
            fold = fold.wrapping_add(hash_one(k) ^ hash_one(v).rotate_left(9));
        }
        fold.hash(&mut h);
        h.finish()
    }

    /// The memoized front door to `prebuild_workspace_lines` (#1090). On a
    /// signature match — the common case, since a streaming agent triggers a
    /// flood of `TerminalOutput` redraws that never touch sidebar state — it
    /// clones the cached lines and skips the whole per-row `build_row` +
    /// `render_table` pass. A miss rebuilds and re-stores.
    pub(crate) fn cached_workspace_lines(
        &mut self,
        row_budget: usize,
        theme: &crate::theme::Theme,
        now: chrono::DateTime<chrono::Utc>,
        focused: bool,
        window: std::ops::Range<usize>,
    ) -> std::rc::Rc<Vec<Option<Line<'static>>>> {
        let sig = self.workspace_lines_signature(row_budget, focused, now, &window);
        if let Some((cached_sig, lines)) = &self.workspace_line_cache {
            if *cached_sig == sig {
                // Rc, not a deep clone (2026-08-19 audit, U2): a HIT
                // used to clone every span and its owned String — ~600
                // allocations per frame — which re-created most of the
                // cost the memo existed to remove. The consumer clones
                // only the handful of viewport rows it actually paints.
                return std::rc::Rc::clone(lines);
            }
        }
        let lines = std::rc::Rc::new(
            self.prebuild_workspace_lines(row_budget, theme, now, focused, &window),
        );
        #[cfg(test)]
        self.workspace_line_builds
            .set(self.workspace_line_builds.get() + 1);
        self.workspace_line_cache = Some((sig, std::rc::Rc::clone(&lines)));
        lines
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
        window: &std::ops::Range<usize>,
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
        // Only groups that intersect the viewport window get laid out —
        // off-screen groups' slots stay `None`, which the consumer never
        // reads (it walks exactly the window). A group that intersects
        // is built IN FULL, so its column widths are sized over all of
        // its rows and stay stable as it scrolls through the viewport
        // (#22 / #961). Indices in a group are ascending, so the
        // overlap test is a range check on its endpoints.
        let intersects = |group: &[usize]| {
            group
                .first()
                .zip(group.last())
                .is_some_and(|(&first, &last)| first < window.end && last >= window.start)
        };
        // Partition workspace rows into groups delimited by their
        // top-level container header — a repo group, or the synthetic
        // Focused pin. A `KindHeader` (PRs vs Issues) doesn't split the
        // group: it stays within one repo, so one provider. Session
        // sub-rows are skipped and don't break the run.
        let mut group: Vec<usize> = Vec::new();
        for (i, row) in self.visible.iter().enumerate() {
            match row {
                VisibleRow::FocusedHeader | VisibleRow::RepoHeader(_) => {
                    if intersects(&group) {
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
                    }
                    group.clear();
                }
                VisibleRow::Workspace(_) => group.push(i),
                _ => {}
            }
        }
        if intersects(&group) {
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
        }
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
            // The search term to underline in this row's title. `render` is
            // per-frame, so the "does the search touch this row?" decision is
            // precomputed into `searched_keys` at recompute time (#1099) —
            // here it's an O(1) lookup, and it can't diverge from the filter.
            let highlight_query: Option<&str> = self.search.as_ref().and_then(|s| {
                let q = crate::components::visible_rows::normalized_query(&s.query);
                (!q.is_empty() && self.searched_keys.contains(key)).then_some(q)
            });
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
                credit_exhausted: workspace.is_some_and(|w| {
                    crate::agent_attention::workspace_is_credit_exhausted(w, &self.agents)
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
                sent_snippet_count: workspace.map_or(0, |w| w.sent_snippets.total()),
                // Source-attention ladder (#scale): a row in a Quiet /
                // Digest / Muted source drops its ambient unread badge
                // unless it punches through (direct address).
                recently_woken: workspace.is_some_and(|w| w.is_recently_woken(now)),
                source_quiet: workspace.is_some_and(|w| {
                    let label = crate::components::visible_rows::group_label(
                        w,
                        &self.projects,
                        &self.workspaces,
                    );
                    self.repo_summaries
                        .get(&label)
                        .and_then(|s| s.source_attention.as_deref())
                        .is_some()
                        && !lazybox_tui_core::inbox::punches_through(w, &self.agents)
                }),
                ticket_tree: self.ticket_tree.get(key).copied(),
                stack: self.stacks.get(key),
                model_shorts: &self.model_shorts,
                highlight_query,
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
