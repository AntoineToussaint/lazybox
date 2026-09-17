//! `Legend` — the sidebar glyph legend (default `Shift-I`, #1744).
//!
//! Renders `lazybox_tui_core::markers` — the generated marker registry
//! that already feeds the Ask Lazybox context — as a scrollable reader,
//! grouped status / agent state / row badges / repo header, each glyph
//! painted the way the rows paint it beside its one-line meaning. There
//! is no copy of its own: the registry is the single source, so the
//! legend inherits `documented_status_pills_match_the_renderer` and a
//! pill can't reach the sidebar without a row here. Only the *tones* are
//! decided client-side, because only the UI can see the active theme.
//!
//! Navigation keys scroll; `?` returns to the Shortcuts panel; any other
//! key dismisses.

use crate::components::sidebar::{
    ARM_GLYPH, AUTO_GLYPH, CLAIM_GLYPH, FIX_GLYPH, G_ISSUE, G_PR, G_TICKET, TRACK_GLYPH,
    pill_for_tag_in, role_badge,
};
use crate::realm::components::scrollable::{
    centered_rect, draw_frame, handle_scroll_key, max_scroll,
};
use crate::realm::{Msg, UserEvent};
use crate::theme::Theme;
use crate::util::visual_width;
use lazybox_core::{StatusTag, TaskRole};
use lazybox_ipc::AgentState;
use lazybox_tui_core::markers;
use tuirealm::command::{Cmd, CmdResult};
use tuirealm::component::{AppComponent, Component};
use tuirealm::event::{Event, Key};
use tuirealm::props::{AttrValue, Attribute, QueryResult};
use tuirealm::ratatui::Frame;
use tuirealm::ratatui::layout::Rect;
use tuirealm::ratatui::prelude::*;
use tuirealm::ratatui::widgets::Paragraph;
use tuirealm::state::State;

/// One legend row: the marker as it renders on screen, the tone it
/// renders in, and the registry's one-line meaning.
pub(crate) struct LegendRow {
    pub(crate) glyph: String,
    pub(crate) style: Style,
    pub(crate) meaning: &'static str,
}

/// One titled group of rows.
pub(crate) struct LegendGroup {
    pub(crate) title: &'static str,
    pub(crate) rows: Vec<LegendRow>,
}

/// The glyph-legend window.
pub(crate) struct Legend {
    groups: Vec<LegendGroup>,
    /// Topmost visible body line.
    scroll: u16,
    /// Body viewport height, cached in `view` for page jumps.
    body_height: u16,
}

impl Legend {
    /// Build from the marker registry, painted in the active theme.
    pub(crate) fn from_registry() -> Self {
        Self {
            groups: legend_groups(crate::theme::current()),
            scroll: 0,
            body_height: 0,
        }
    }

    /// The scrollable body: a title line per group, then each row as
    /// `glyph  meaning` with the meaning word-wrapped under itself so a
    /// long registry entry never clips. The glyph column is sized per
    /// group and capped: a label wider than the cap (the descriptive
    /// row-badge ones, `Runner letter (C / X / U / S)…`) takes a line of
    /// its own with the meaning wrapped beneath it, rather than pushing
    /// every meaning in the group to the right edge. Re-derived each
    /// render so width and theme changes are picked up.
    fn body_lines(&self, width: usize, theme: &Theme) -> Vec<Line<'static>> {
        const GLYPH_COL_MAX: usize = 24;
        let mut lines: Vec<Line<'static>> = Vec::new();
        let title_style = Style::default()
            .fg(theme.accent)
            .add_modifier(Modifier::BOLD);
        let text = Style::default().fg(theme.text_strong);
        for (idx, group) in self.groups.iter().enumerate() {
            if idx > 0 {
                lines.push(Line::default());
            }
            lines.push(Line::from(Span::styled(group.title, title_style)));
            let glyph_col = group
                .rows
                .iter()
                .map(|r| visual_width(&r.glyph))
                .max()
                .unwrap_or(1)
                .min(GLYPH_COL_MAX)
                .min(width.saturating_sub(12).max(1));
            let indent = 1 + glyph_col + 2;
            let wrap_to = width.saturating_sub(indent).max(8);
            for row in &group.rows {
                let glyph_width = visual_width(&row.glyph);
                let mut wrapped = wrap_words(row.meaning, wrap_to).into_iter();
                if glyph_width <= glyph_col {
                    let first = wrapped.next().unwrap_or_default();
                    lines.push(Line::from(vec![
                        Span::raw(" "),
                        Span::styled(row.glyph.clone(), row.style),
                        Span::raw(" ".repeat(glyph_col - glyph_width + 2)),
                        Span::styled(first, text),
                    ]));
                } else {
                    lines.push(Line::from(vec![
                        Span::raw(" "),
                        Span::styled(row.glyph.clone(), row.style),
                    ]));
                }
                for rest in wrapped {
                    lines.push(Line::from(vec![
                        Span::raw(" ".repeat(indent)),
                        Span::styled(rest, text),
                    ]));
                }
            }
        }
        lines
    }
}

/// Greedy word wrap on whitespace, measured in cells. A single word
/// longer than `width` gets its own line rather than being split.
fn wrap_words(text: &str, width: usize) -> Vec<String> {
    let mut lines: Vec<String> = Vec::new();
    let mut current = String::new();
    for word in text.split_whitespace() {
        if current.is_empty() {
            current.push_str(word);
        } else if visual_width(&current) + 1 + visual_width(word) <= width {
            current.push(' ');
            current.push_str(word);
        } else {
            lines.push(std::mem::take(&mut current));
            current.push_str(word);
        }
    }
    if !current.is_empty() {
        lines.push(current);
    }
    lines
}

/// The registry rendered into groups, painted in `theme`. Each glyph
/// carries the tone the sidebar gives it: status pills straight from the
/// pill renderer, agent states per `workspace_row::cell_state`, the
/// policy badges by their glyph constant, and the header's kind / role
/// tokens as the row's type glyph and role letter.
pub(crate) fn legend_groups(theme: &Theme) -> Vec<LegendGroup> {
    let bold = |c: Color| Style::default().fg(c).add_modifier(Modifier::BOLD);
    let row = |glyph: String, style: Style, meaning: &'static str| LegendRow {
        glyph,
        style,
        meaning,
    };

    let status = StatusTag::ALL
        .into_iter()
        .filter_map(|tag| {
            let doc = markers::status_pill_doc(tag)?;
            let style = pill_for_tag_in(tag, theme).map_or(bold(theme.text_strong), |p| p.style);
            Some(row(doc.label.trim().to_string(), style, doc.meaning))
        })
        .collect();

    let agent_style = |state: &AgentState| match state {
        AgentState::InputNeeded | AgentState::LimitReached | AgentState::CreditExhausted => {
            bold(theme.warn)
        }
        AgentState::Working => bold(theme.accent),
        AgentState::Done => bold(theme.success),
        AgentState::Idle | AgentState::Exited { .. } | AgentState::AwaitingReset => {
            Style::default().fg(theme.text_dim)
        }
    };
    let mut agent: Vec<LegendRow> = AgentState::ALL
        .iter()
        .map(|state| {
            let doc = markers::agent_state_doc(state);
            row(doc.label.to_string(), agent_style(state), doc.meaning)
        })
        .collect();
    let spawning = markers::spawning_doc();
    agent.push(row(
        spawning.label.to_string(),
        Style::default().fg(theme.text_dim),
        spawning.meaning,
    ));

    let badges = markers::row_badge_docs()
        .iter()
        .map(|doc| {
            let style = match doc.label {
                // The row paints ARM as a filled success pill, not a
                // coloured glyph — mirror it so the swatch is the badge.
                l if l == ARM_GLYPH => Style::default().bg(theme.success).fg(Color::Black),
                l if l == AUTO_GLYPH || l == TRACK_GLYPH => bold(theme.accent),
                l if l == FIX_GLYPH || l == CLAIM_GLYPH => bold(theme.warn),
                _ => bold(theme.text_strong),
            };
            row(doc.label.to_string(), style, doc.meaning)
        })
        .collect();

    let header = markers::header_breakdown_docs()
        .iter()
        .map(|doc| {
            let glyph = doc.label.trim_start_matches('N');
            let color = match glyph {
                g if g == G_PR => theme.success,
                g if g == G_ISSUE => theme.text_strong,
                g if g == G_TICKET => theme.accent,
                "A" => role_badge(theme, TaskRole::Author).1,
                "R" => role_badge(theme, TaskRole::Reviewer).1,
                "@" => role_badge(theme, TaskRole::Assignee).1,
                _ => theme.text_strong,
            };
            row(
                doc.label.to_string(),
                Style::default().fg(color),
                doc.meaning,
            )
        })
        .collect();

    vec![
        LegendGroup {
            title: "Status — the row's right-side pills",
            rows: status,
        },
        LegendGroup {
            title: "Agent state — the row's session glyph and terminal tab badge",
            rows: agent,
        },
        LegendGroup {
            title: "Row badges — automation, role, runner, model",
            rows: badges,
        },
        LegendGroup {
            title: "Repo header — what the group is made of (N = count)",
            rows: header,
        },
    ]
}

impl Component for Legend {
    fn view(&mut self, frame: &mut Frame, area: Rect) {
        let theme = crate::theme::current();
        let modal_w = 96u16.min(area.width.saturating_sub(4));
        let modal_h = 40u16.min(area.height.saturating_sub(2));
        let modal = centered_rect(area, modal_w, modal_h);
        let inner = draw_frame(frame, modal, " Legend ", theme);
        if inner.height < 2 {
            return;
        }

        let body_area = Rect {
            x: inner.x,
            y: inner.y,
            width: inner.width,
            height: inner.height - 1,
        };
        let hint_area = Rect {
            x: inner.x,
            y: inner.y + inner.height - 1,
            width: inner.width,
            height: 1,
        };
        self.body_height = body_area.height.max(1);

        let lines = self.body_lines(body_area.width as usize, theme);
        let max = max_scroll(lines.len(), self.body_height);
        if self.scroll > max {
            self.scroll = max;
        }
        frame.render_widget(
            Paragraph::new(lines)
                .style(Style::default().bg(theme.surface))
                .scroll((self.scroll, 0)),
            body_area,
        );
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                "↑/↓ scroll · ? shortcuts · any other key to close",
                theme.hint(),
            ))),
            hint_area,
        );
    }

    fn query(&self, _: Attribute) -> Option<QueryResult<'_>> {
        None
    }
    fn attr(&mut self, _: Attribute, _: AttrValue) {}
    fn state(&self) -> State {
        State::None
    }
    fn perform(&mut self, _: Cmd) -> CmdResult {
        CmdResult::NoChange
    }
}

impl AppComponent<Msg, UserEvent> for Legend {
    fn on(&mut self, ev: &Event<UserEvent>) -> Option<Msg> {
        let Event::Keyboard(key) = ev else {
            return None;
        };
        if handle_scroll_key(&mut self.scroll, self.body_height, key) {
            return None;
        }
        match key.code {
            Key::End | Key::Char('G') => {
                self.scroll = u16::MAX;
                None
            }
            Key::Char('?') => Some(Msg::HelpIndexOpen),
            _ => Some(Msg::ModalDismissed),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tuirealm::event::{KeyEvent, KeyModifiers};
    use tuirealm::ratatui::Terminal;
    use tuirealm::ratatui::backend::TestBackend;

    fn press(code: Key) -> Event<UserEvent> {
        Event::Keyboard(KeyEvent::new(code, KeyModifiers::NONE))
    }

    fn render(legend: &mut Legend, w: u16, h: u16) -> String {
        let mut term = Terminal::new(TestBackend::new(w, h)).unwrap();
        term.draw(|f| legend.view(f, f.area())).unwrap();
        let buf = term.backend().buffer();
        (0..buf.area.height)
            .map(|y| {
                (0..buf.area.width)
                    .map(|x| buf[(x, y)].symbol().to_string())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn all_rows(theme: &Theme) -> Vec<LegendRow> {
        legend_groups(theme)
            .into_iter()
            .flat_map(|g| g.rows)
            .collect()
    }

    /// An explicit palette rather than the process-global active theme:
    /// the theme-picker tests switch that one under sibling threads
    /// (#1751), so sampling it twice in one test is a race.
    fn light() -> &'static Theme {
        crate::theme::list()
            .into_iter()
            .find(|t| t.name == "Lazybox Light")
            .expect("light theme must exist")
    }

    /// The legend is the registry, whole: every status pill, agent
    /// state, row badge and header token the docs know about has a row,
    /// each carrying the registry's own meaning — no copy of its own.
    #[test]
    fn every_registry_entry_has_a_row() {
        let rows = all_rows(light());
        let has = |label: &str, meaning: &str| {
            rows.iter()
                .any(|r| r.glyph == label.trim() && r.meaning == meaning)
        };
        for doc in markers::status_pill_docs() {
            assert!(
                has(doc.label, doc.meaning),
                "status pill {} missing",
                doc.label
            );
        }
        for doc in markers::agent_state_docs() {
            assert!(
                has(doc.label, doc.meaning),
                "agent state {} missing",
                doc.label
            );
        }
        let spawning = markers::spawning_doc();
        assert!(
            has(spawning.label, spawning.meaning),
            "spawning arc missing"
        );
        for doc in markers::row_badge_docs() {
            assert!(
                has(doc.label, doc.meaning),
                "row badge {} missing",
                doc.label
            );
        }
        for doc in markers::header_breakdown_docs() {
            assert!(
                has(doc.label, doc.meaning),
                "header token {} missing",
                doc.label
            );
        }
        let documented = markers::status_pill_docs().len()
            + markers::agent_state_docs().len()
            + 1
            + markers::row_badge_docs().len()
            + markers::header_breakdown_docs().len();
        assert_eq!(rows.len(), documented, "no rows beyond the registry");
    }

    /// A status glyph takes the exact style the pill renderer paints it
    /// with, so `✓` approved and `✓` ready differ by tone in the legend
    /// the way they do on a row.
    #[test]
    fn status_rows_use_the_pill_renderers_tone() {
        let theme = light();
        let rows = all_rows(theme);
        for tag in StatusTag::ALL {
            let Some(doc) = markers::status_pill_doc(tag) else {
                continue;
            };
            let pill = pill_for_tag_in(tag, theme).expect("documented pill renders");
            let row = rows
                .iter()
                .find(|r| r.glyph == doc.label.trim() && r.meaning == doc.meaning)
                .expect("row present");
            assert_eq!(row.style, pill.style, "{:?} tone differs", tag);
        }
    }

    /// Every swatch carries a theme-derived RGB tone — the #1046 rule
    /// that a fixed palette index is unreadable on the light surface. A
    /// filled pill (ARM) carries it as its background.
    #[test]
    fn swatches_are_theme_tones() {
        for row in all_rows(light()) {
            let tone = row
                .style
                .bg
                .or(row.style.fg)
                .expect("every glyph has a tone");
            assert!(
                matches!(tone, Color::Rgb(..)),
                "{}: {tone:?} is not a theme tone",
                row.glyph
            );
        }
        let arm = all_rows(light())
            .into_iter()
            .find(|r| r.glyph == ARM_GLYPH)
            .expect("ARM row");
        assert_eq!(
            arm.style.bg,
            Some(light().success),
            "ARM is the row's filled pill"
        );
    }

    /// The body carries all four groups in order, long registry
    /// meanings wrap under themselves instead of clipping, and the
    /// framed window renders with the first group visible.
    #[test]
    fn renders_groups_and_wraps_long_meanings() {
        let mut legend = Legend::from_registry();
        let out = render(&mut legend, 100, 60);
        assert!(out.contains("Legend"), "title: {out}");
        assert!(out.contains("Status"), "first group visible: {out}");
        let lines = legend.body_lines(60, light());
        let titles: Vec<usize> = ["Status", "Agent state", "Row badges", "Repo header"]
            .iter()
            .map(|title| {
                lines
                    .iter()
                    .position(|l| l.to_string().starts_with(title))
                    .unwrap_or_else(|| panic!("missing group {title}"))
            })
            .collect();
        assert!(titles.windows(2).all(|w| w[0] < w[1]), "groups in order");
        let longest = markers::agent_state_docs()
            .into_iter()
            .map(|d| d.meaning)
            .max_by_key(|m| m.len())
            .unwrap();
        assert!(
            lines.len() > all_rows(light()).len() + 4,
            "a {}-char meaning must wrap onto continuation lines",
            longest.len()
        );
        for line in &lines {
            assert!(
                line.width() <= 60,
                "wrapped line overflows the body: {line:?}"
            );
        }
        // A label wider than the glyph column sits on its own line, its
        // meaning wrapped underneath rather than squeezed to the edge.
        let long = markers::row_badge_docs()
            .iter()
            .map(|d| d.label)
            .max_by_key(|l| visual_width(l))
            .unwrap();
        assert!(
            visual_width(long) > 24,
            "fixture: a label wider than the cap"
        );
        let at = lines
            .iter()
            .position(|l| l.to_string().trim() == long)
            .expect("the long label has a line of its own");
        assert!(
            lines[at + 1].to_string().starts_with(&" ".repeat(20)),
            "its meaning is indented beneath it: {:?}",
            lines[at + 1]
        );
    }

    #[test]
    fn wrap_words_packs_greedily_and_keeps_long_words_whole() {
        assert_eq!(wrap_words("a bb ccc dddd", 6), ["a bb", "ccc", "dddd"]);
        assert_eq!(
            wrap_words("supercalifragilistic x", 5),
            ["supercalifragilistic", "x"]
        );
        assert!(wrap_words("", 5).is_empty());
    }

    /// Scroll keys are consumed; `?` swaps back to the shortcuts panel;
    /// anything else closes the window.
    #[test]
    fn keys_scroll_swap_or_dismiss() {
        let mut legend = Legend::from_registry();
        legend.body_height = 10;
        assert!(legend.on(&press(Key::Char('j'))).is_none());
        assert_eq!(legend.scroll, 1);
        assert!(legend.on(&press(Key::PageDown)).is_none());
        assert_eq!(legend.scroll, 11);
        assert!(legend.on(&press(Key::End)).is_none());
        assert_eq!(legend.scroll, u16::MAX);
        assert!(legend.on(&press(Key::Home)).is_none());
        assert_eq!(legend.scroll, 0);
        assert!(matches!(
            legend.on(&press(Key::Char('?'))),
            Some(Msg::HelpIndexOpen)
        ));
        assert!(matches!(
            legend.on(&press(Key::Esc)),
            Some(Msg::ModalDismissed)
        ));
        assert!(matches!(
            legend.on(&press(Key::Char('q'))),
            Some(Msg::ModalDismissed)
        ));
    }

    /// An over-scroll is clamped at render so the last line stays on
    /// screen.
    #[test]
    fn scroll_is_clamped_to_content() {
        let mut legend = Legend::from_registry();
        legend.scroll = u16::MAX;
        let _ = render(&mut legend, 100, 30);
        let total = legend.body_lines(94, light()).len();
        assert_eq!(legend.scroll, max_scroll(total, legend.body_height));
        assert!(legend.scroll < u16::MAX);
    }
}
