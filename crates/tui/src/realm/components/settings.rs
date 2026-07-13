//! `Settings` — the tabbed settings window (`,`).
//!
//! Replaces the old flat `Choice` palette, which piled 11+ rows (two
//! more per enabled provider) into one undifferentiated list where
//! "Change theme" sat next to "Clean worktrees". Rows are grouped
//! under four tabs (`SettingsSection`): Providers / Agents /
//! Appearance / Maintenance — `←/→` (or `h/l`, Tab) switches tabs,
//! `j/k` moves within one, Enter picks, `1-4` jumps straight to a tab.
//!
//! The component is presentation-only: rows arrive as
//! `(label, flat_index)` pairs per tab, and Enter emits
//! `Msg::ChoicePicked([flat_index])` — the exact envelope the old
//! palette produced — so the model's existing
//! `settings_actions[idx] → dispatch_settings_action` routing is
//! untouched. Empty tabs (no enabled providers yet) are skipped when
//! cycling but still shown greyed in the tab bar so the shape of the
//! settings space stays visible.

use crate::realm::setup_ctx::SettingsSection;
use crate::realm::{Msg, UserEvent};
use tuirealm::command::{Cmd, CmdResult};
use tuirealm::component::{AppComponent, Component};
use tuirealm::event::{Event, Key};
use tuirealm::props::{AttrValue, Attribute, QueryResult};
use tuirealm::ratatui::Frame;
use tuirealm::ratatui::layout::Rect;
use tuirealm::ratatui::prelude::*;
use tuirealm::ratatui::widgets::{Block, BorderType, Borders, Clear, Paragraph};
use tuirealm::state::State;

/// One tab's worth of rows: the section it renders plus
/// `(display label, index into the model's flat settings_actions)`.
pub(crate) struct SettingsTab {
    pub section: SettingsSection,
    pub rows: Vec<(String, usize)>,
}

/// Tabbed settings window.
pub(crate) struct Settings {
    tabs: Vec<SettingsTab>,
    /// Active tab index into `tabs`.
    active: usize,
    /// Cursor row within the active tab.
    cursor: usize,
}

impl Settings {
    /// Build from pre-grouped tabs. Starts on the first non-empty tab
    /// so a fresh install (no providers configured yet) doesn't open
    /// onto a blank pane.
    pub(crate) fn new(tabs: Vec<SettingsTab>) -> Self {
        let active = tabs.iter().position(|t| !t.rows.is_empty()).unwrap_or(0);
        Self {
            tabs,
            active,
            cursor: 0,
        }
    }

    fn active_rows(&self) -> &[(String, usize)] {
        self.tabs
            .get(self.active)
            .map(|t| t.rows.as_slice())
            .unwrap_or(&[])
    }

    /// Move to the next/previous tab with rows, wrapping. Cursor
    /// resets — row positions don't correspond across tabs.
    fn switch_tab(&mut self, delta: isize) {
        let n = self.tabs.len() as isize;
        if n == 0 {
            return;
        }
        let mut idx = self.active as isize;
        for _ in 0..n {
            idx = (idx + delta).rem_euclid(n);
            if !self.tabs[idx as usize].rows.is_empty() {
                break;
            }
        }
        if idx as usize != self.active {
            self.active = idx as usize;
            self.cursor = 0;
        }
    }

    /// Jump straight to tab `n` (0-based) when it has rows.
    fn jump_tab(&mut self, n: usize) {
        if self.tabs.get(n).is_some_and(|t| !t.rows.is_empty()) && n != self.active {
            self.active = n;
            self.cursor = 0;
        }
    }

    fn move_cursor(&mut self, delta: isize) {
        let len = self.active_rows().len();
        if len == 0 {
            return;
        }
        let cur = self.cursor as isize;
        self.cursor = (cur + delta).rem_euclid(len as isize) as usize;
    }

    /// The tab bar: ` 1 Providers │ 2 Agents │ … ` with the active tab
    /// accent-highlighted and empty tabs dimmed.
    fn tab_bar(&self, theme: &crate::theme::Theme) -> Line<'static> {
        let mut spans: Vec<Span> = Vec::with_capacity(self.tabs.len() * 2);
        for (i, tab) in self.tabs.iter().enumerate() {
            if i > 0 {
                spans.push(Span::styled("  │  ", Style::default().fg(theme.chrome)));
            }
            let label = format!("{} {}", i + 1, tab.section.title());
            let style = if i == self.active {
                Style::default()
                    .fg(theme.accent)
                    .add_modifier(Modifier::BOLD)
            } else if tab.rows.is_empty() {
                Style::default()
                    .fg(theme.text_dim)
                    .add_modifier(Modifier::DIM)
            } else {
                Style::default().fg(theme.text_strong)
            };
            spans.push(Span::styled(label, style));
        }
        Line::from(spans)
    }
}

impl Component for Settings {
    fn view(&mut self, frame: &mut Frame, area: Rect) {
        let theme = crate::theme::current();
        // Height: tab bar + divider + tallest tab + hint, bounded to
        // the viewport. Width fits the longest row with margin.
        let tallest = self.tabs.iter().map(|t| t.rows.len()).max().unwrap_or(0) as u16;
        // Width fits the longest row AND the full tab bar — a
        // truncated tab strip would hide whole sections.
        let tab_bar_w = self
            .tabs
            .iter()
            .map(|t| t.section.title().chars().count() as u16 + 2)
            .sum::<u16>()
            + (self.tabs.len().saturating_sub(1) as u16) * 5;
        let widest = self
            .tabs
            .iter()
            .flat_map(|t| t.rows.iter())
            .map(|(l, _)| l.chars().count() as u16)
            .max()
            .unwrap_or(0)
            .max(tab_bar_w);
        let modal_w = (widest + 8).clamp(46, area.width.saturating_sub(4)).max(20);
        let modal_h = (tallest + 6).min(area.height.saturating_sub(2)).max(6);
        let x = area.x + area.width.saturating_sub(modal_w) / 2;
        let y = area.y + area.height.saturating_sub(modal_h) / 2;
        let modal = Rect::new(x, y, modal_w, modal_h);

        frame.render_widget(Clear, modal);
        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(theme.modal_border())
            .title(" Settings ")
            .title_style(theme.modal_title());
        let inner = block.inner(modal);
        frame.render_widget(block, modal);
        if inner.height < 3 {
            return;
        }

        let mut lines: Vec<Line> = Vec::with_capacity(inner.height as usize);
        lines.push(self.tab_bar(theme));
        lines.push(Line::from(Span::styled(
            "─".repeat(inner.width as usize),
            Style::default().fg(theme.chrome),
        )));
        for (i, (label, _)) in self.active_rows().iter().enumerate() {
            let (caret, style) = if i == self.cursor {
                ("▸ ", theme.row_focused())
            } else {
                ("  ", Style::default().fg(theme.text_strong))
            };
            lines.push(Line::from(vec![
                Span::styled(caret, Style::default().fg(theme.accent)),
                Span::styled(label.clone(), style),
            ]));
        }
        if self.active_rows().is_empty() {
            lines.push(Line::from(Span::styled(
                "nothing to configure here yet",
                Style::default().fg(theme.text_dim),
            )));
        }

        // Body (everything but the bottom hint row).
        let body = Rect {
            x: inner.x,
            y: inner.y,
            width: inner.width,
            height: inner.height - 1,
        };
        frame.render_widget(Paragraph::new(lines), body);
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                "←/→ tab · ↑/↓ move · Enter pick · Esc close",
                theme.hint(),
            ))),
            Rect {
                x: inner.x,
                y: inner.y + inner.height - 1,
                width: inner.width,
                height: 1,
            },
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

impl AppComponent<Msg, UserEvent> for Settings {
    fn on(&mut self, ev: &Event<UserEvent>) -> Option<Msg> {
        let Event::Keyboard(key) = ev else {
            return None;
        };
        match key.code {
            Key::Right | Key::Char('l') | Key::Tab => {
                self.switch_tab(1);
                None
            }
            Key::Left | Key::Char('h') | Key::BackTab => {
                self.switch_tab(-1);
                None
            }
            Key::Down | Key::Char('j') => {
                self.move_cursor(1);
                None
            }
            Key::Up | Key::Char('k') => {
                self.move_cursor(-1);
                None
            }
            Key::Home | Key::Char('g') => {
                self.cursor = 0;
                None
            }
            Key::End | Key::Char('G') => {
                self.cursor = self.active_rows().len().saturating_sub(1);
                None
            }
            // Digit shortcuts mirror the numbers painted on the tab bar.
            Key::Char(c @ '1'..='9') => {
                self.jump_tab(c as usize - '1' as usize);
                None
            }
            Key::Enter => self
                .active_rows()
                .get(self.cursor)
                .map(|(_, flat_idx)| Msg::ChoicePicked(vec![*flat_idx])),
            Key::Esc | Key::Char('q') => Some(Msg::ModalDismissed),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tuirealm::event::{KeyEvent, KeyModifiers};

    fn tabs() -> Vec<SettingsTab> {
        vec![
            SettingsTab {
                section: SettingsSection::Providers,
                rows: vec![
                    ("Add / remove repos · GitHub".into(), 0),
                    ("Edit roles + filters · GitHub".into(), 1),
                    ("Edit providers (github / linear / …)".into(), 2),
                ],
            },
            SettingsTab {
                section: SettingsSection::Agents,
                rows: vec![
                    ("Change default agent · claude".into(), 3),
                    ("Configure LLM gateway · off".into(), 4),
                ],
            },
            SettingsTab {
                section: SettingsSection::Appearance,
                rows: vec![("Change theme (live preview) · Dark".into(), 5)],
            },
            SettingsTab {
                section: SettingsSection::Maintenance,
                rows: vec![("Clean worktrees (free disk, keep inbox)".into(), 6)],
            },
        ]
    }

    fn key(code: Key) -> Event<UserEvent> {
        Event::Keyboard(KeyEvent {
            code,
            modifiers: KeyModifiers::NONE,
        })
    }

    fn render(comp: &mut Settings, w: u16, h: u16) -> String {
        use tuirealm::ratatui::Terminal;
        use tuirealm::ratatui::backend::TestBackend;
        let backend = TestBackend::new(w, h);
        let mut term = Terminal::new(backend).unwrap();
        term.draw(|frame| comp.view(frame, Rect::new(0, 0, w, h)))
            .unwrap();
        let buf = term.backend().buffer();
        (0..buf.area.height)
            .map(|y| {
                let mut row = String::new();
                for x in 0..buf.area.width {
                    row.push_str(buf[(x, y)].symbol());
                }
                row.trim_end().to_string()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn renders_tab_bar_and_active_tabs_rows_only() {
        let mut comp = Settings::new(tabs());
        let out = render(&mut comp, 90, 20);
        // All four tab titles visible.
        for title in ["Providers", "Agents", "Appearance", "Maintenance"] {
            assert!(out.contains(title), "missing tab {title}: {out}");
        }
        // Active (first) tab's rows visible; other tabs' rows are not.
        assert!(out.contains("Add / remove repos · GitHub"), "{out}");
        assert!(!out.contains("Change theme"), "{out}");
        assert!(!out.contains("Clean worktrees"), "{out}");
    }

    #[test]
    fn tab_switch_shows_that_tabs_rows_and_resets_cursor() {
        let mut comp = Settings::new(tabs());
        assert_eq!(comp.on(&key(Key::Down)), None);
        assert_eq!(comp.cursor, 1);

        assert_eq!(comp.on(&key(Key::Right)), None);
        assert_eq!(comp.active, 1);
        assert_eq!(comp.cursor, 0, "cursor resets on tab switch");
        let out = render(&mut comp, 90, 20);
        assert!(out.contains("Change default agent"), "{out}");
        assert!(!out.contains("Add / remove repos"), "{out}");

        // Wraps backwards from the first tab to the last.
        assert_eq!(comp.on(&key(Key::Left)), None);
        assert_eq!(comp.on(&key(Key::Left)), None);
        assert_eq!(comp.active, 3, "left from tab 0 wraps to Maintenance");
    }

    #[test]
    fn enter_emits_the_flat_index_of_the_cursor_row() {
        let mut comp = Settings::new(tabs());
        // Tab 2 (Agents), row 1 → flat index 4.
        assert_eq!(comp.on(&key(Key::Char('2'))), None);
        assert_eq!(comp.on(&key(Key::Char('j'))), None);
        assert_eq!(
            comp.on(&key(Key::Enter)),
            Some(Msg::ChoicePicked(vec![4])),
            "Enter must emit the model's flat settings_actions index"
        );
    }

    #[test]
    fn esc_dismisses() {
        let mut comp = Settings::new(tabs());
        assert_eq!(comp.on(&key(Key::Esc)), Some(Msg::ModalDismissed));
    }

    #[test]
    fn empty_tabs_are_skipped_when_cycling() {
        let mut t = tabs();
        t[1].rows.clear(); // Agents empty
        let mut comp = Settings::new(t);
        assert_eq!(comp.active, 0);
        comp.on(&key(Key::Right));
        assert_eq!(comp.active, 2, "cycling skips the empty Agents tab");
        // Digit jump to an empty tab is a no-op.
        comp.on(&key(Key::Char('2')));
        assert_eq!(comp.active, 2);
    }

    #[test]
    fn starts_on_first_non_empty_tab() {
        let mut t = tabs();
        t[0].rows.clear();
        let comp = Settings::new(t);
        assert_eq!(comp.active, 1, "must not open onto a blank pane");
    }
}
