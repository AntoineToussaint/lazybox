//! Client-local session switcher. Opening it overlays, never resizes, the PTY.
use super::mobile_sessions::SessionRow;
use lazybox_ipc::TerminalId;
use tuirealm::{
    event::{Key, KeyEvent, KeyModifiers},
    ratatui::{
        Frame,
        layout::Rect,
        style::{Modifier, Style},
        text::{Line, Span},
        widgets::{Block, Borders, Clear, Paragraph},
    },
};

// Navigation, creation, rename, priority and delete consume letters. Keep the map shared by
// keyboard lookup and both render modes so labels can never disagree.
const SELECTORS: &[u8] = b"abcdefghilmoqstuvwyz";
const LETTERS: usize = SELECTORS.len();

#[derive(Default)]
pub(crate) struct MobileRail {
    open: bool,
    priority: Option<TerminalId>,
    initialized: bool,
    // Snapshot identities, not positions in the live list: a spawn/exit must
    // never change what the next letter will select while this is open.
    targets: Vec<TerminalId>,
    group: usize,
    scroll: usize,
    cursor: usize,
    visible: usize,
    panel: Rect,
    items: Rect,
}

#[derive(Clone, Copy)]
pub(crate) enum RailAction {
    None,
    Close,
    Select(TerminalId),
    New,
    Links(TerminalId),
    Prioritize {
        source: TerminalId,
        target: TerminalId,
    },
    Rename(TerminalId),
    Delete(TerminalId),
    Quit,
}

impl MobileRail {
    pub(crate) fn is_open(&self) -> bool {
        self.open
    }

    pub(crate) fn open(&mut self, rows: &[SessionRow]) {
        self.priority = None;
        self.targets = rows.iter().map(|r| r.terminal_id).collect();
        self.group = 0;
        self.scroll = 0;
        self.cursor = 0;
        self.open = true;
        self.initialized = true;
    }

    /// Refresh the displayed roster, preserving the highlighted identity.
    /// Keys resolve against this painted snapshot, never a freshly reordered list.
    pub(crate) fn update(&mut self, rows: &[SessionRow]) {
        self.initialized = true;
        let selected = self.highlighted();
        self.targets = rows.iter().map(|r| r.terminal_id).collect();
        self.cursor = selected
            .and_then(|id| self.targets.iter().position(|t| *t == id))
            .unwrap_or(self.cursor)
            .min(self.targets.len().saturating_sub(1));
        self.group = self.cursor / LETTERS;
    }

    pub(crate) fn initialize(&mut self, rows: &[SessionRow]) {
        if !self.initialized {
            self.update(rows);
        }
    }

    pub(crate) fn highlighted(&self) -> Option<TerminalId> {
        self.targets.get(self.cursor).copied()
    }

    pub(crate) fn highlight_initial(&mut self, id: TerminalId) {
        if let Some(index) = self.targets.iter().position(|t| *t == id) {
            self.cursor = index;
            self.group = index / LETTERS;
        }
    }

    fn move_cursor(&mut self, delta: isize) {
        self.cursor = (self.cursor as isize + delta)
            .clamp(0, self.targets.len().saturating_sub(1) as isize) as usize;
        self.group = self.cursor / LETTERS;
    }

    pub(crate) fn close(&mut self) {
        self.open = false;
        self.priority = None;
    }

    fn group_len(&self) -> usize {
        self.targets
            .len()
            .saturating_sub(self.group * LETTERS)
            .min(LETTERS)
    }

    fn page(&mut self, forward: bool) {
        let last = self.targets.len().saturating_sub(1) / LETTERS;
        self.group = if forward {
            (self.group + 1).min(last)
        } else {
            self.group.saturating_sub(1)
        };
        self.scroll = 0;
        self.cursor = self.group * LETTERS;
    }

    pub(crate) fn scroll(&mut self, delta: isize) {
        self.scroll = (self.scroll as isize + delta).clamp(
            0,
            self.group_len().saturating_sub(self.visible.max(1)) as isize,
        ) as usize;
    }

    pub(crate) fn key(&mut self, key: &KeyEvent) -> RailAction {
        if key.code == Key::Char('q') && key.modifiers == KeyModifiers::CONTROL {
            return RailAction::Quit;
        }
        if key
            .modifiers
            .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
        {
            return RailAction::None;
        }
        match key.code {
            Key::Esc => {
                if self.priority.take().is_none() {
                    return RailAction::Close;
                }
            }
            Key::Enter => {
                return self
                    .highlighted()
                    .map(|id| self.choose(id))
                    .unwrap_or(RailAction::None);
            }
            Key::Char('p') if self.priority.is_none() => self.priority = self.highlighted(),
            Key::Char('n') if self.priority.is_none() => return RailAction::New,
            Key::Char('/') if self.priority.is_none() => {
                return self
                    .highlighted()
                    .map(RailAction::Links)
                    .unwrap_or(RailAction::None);
            }
            Key::Char('r') if self.priority.is_none() => {
                if let Some(id) = self.highlighted() {
                    return RailAction::Rename(id);
                }
            }
            Key::Char('x') if self.priority.is_none() => {
                if let Some(id) = self.highlighted() {
                    return RailAction::Delete(id);
                }
            }
            Key::Char(']') | Key::Tab => self.page(true),
            Key::Char('[') | Key::BackTab => self.page(false),
            Key::Down | Key::Char('j') => self.move_cursor(1),
            Key::Up | Key::Char('k') => self.move_cursor(-1),
            Key::PageDown => self.move_cursor(self.visible.max(1) as isize),
            Key::PageUp => self.move_cursor(-(self.visible.max(1) as isize)),
            Key::Home => self.move_cursor(isize::MIN / 2),
            Key::End => self.move_cursor(isize::MAX / 2),
            Key::Char(c) => {
                if let Some(index) = SELECTORS.iter().position(|letter| char::from(*letter) == c)
                    && let Some(id) = self.targets.get(self.group * LETTERS + index)
                {
                    return self.choose(*id);
                }
            }
            _ => (),
        }
        RailAction::None
    }

    pub(crate) fn is_prioritizing(&self) -> bool {
        self.priority.is_some()
    }

    pub(crate) fn choose(&mut self, target: TerminalId) -> RailAction {
        match self.priority.take() {
            Some(source) => RailAction::Prioritize { source, target },
            None => RailAction::Select(target),
        }
    }

    /// The same action labels and hit regions serve the portal and overlay.
    pub(crate) fn footer(&self) -> &'static str {
        if self.is_prioritizing() {
            "Pick position · Esc cancel"
        } else {
            "n new r name x del p sort / URLs"
        }
    }

    pub(crate) fn footer_key(&self, column: u16) -> Option<KeyEvent> {
        if self.is_prioritizing() {
            return None;
        }
        // Resolve taps from the same labels we render, so shorter mobile
        // labels or new actions cannot silently move their hit regions.
        let mut words = self.footer().split_whitespace();
        let mut offset = 0;
        while let (Some(key), Some(label)) = (words.next(), words.next()) {
            let width = key.len() + 1 + label.len();
            if (offset..offset + width).contains(&usize::from(column)) {
                return key.chars().next().map(|c| KeyEvent::from(Key::Char(c)));
            }
            offset += width + 1;
        }
        None
    }

    pub(crate) fn move_highlight(&mut self, delta: isize) {
        self.move_cursor(delta);
    }

    pub(crate) fn at(&self, x: u16, y: u16) -> Option<TerminalId> {
        if !self.items.contains((x, y).into()) {
            return None;
        }
        let index = self.scroll + usize::from(y - self.items.y);
        if index >= self.group_len() {
            return None;
        }
        self.targets.get(self.group * LETTERS + index).copied()
    }

    pub(crate) fn contains(&self, x: u16, y: u16) -> bool {
        self.panel.contains((x, y).into())
    }

    pub(crate) fn render_collapsed(
        frame: &mut Frame,
        area: Rect,
        rows: &[SessionRow],
        active: Option<TerminalId>,
    ) {
        if area.is_empty() {
            return;
        }
        let theme = crate::theme::current();
        frame.render_widget(
            Paragraph::new(" ").style(Style::default().bg(theme.surface)),
            area,
        );
        let overflow = rows.len() > usize::from(area.height) && area.height >= 3;
        let inset = u16::from(overflow);
        let visible = usize::from(area.height - 2 * inset);
        let position = rows
            .iter()
            .position(|r| Some(r.terminal_id) == active)
            .unwrap_or(0);
        let start = (position + 1)
            .saturating_sub(visible)
            .min(rows.len().saturating_sub(visible));
        for (i, row) in rows.iter().skip(start).take(visible).enumerate() {
            let (glyph, color) = row.indicator();
            let style = Style::default()
                .fg(color)
                .bg(if Some(row.terminal_id) == active {
                    // Selection is neutral chrome; hover can share the error
                    // hue in a theme. Keep state color in the glyph itself.
                    theme.fill
                } else {
                    theme.surface
                });
            frame.render_widget(
                Paragraph::new(glyph).style(style),
                Rect::new(area.x, area.y + inset + i as u16, 1, 1),
            );
        }
        if overflow {
            for (y, glyph) in [
                (area.y, if start > 0 { "↑" } else { " " }),
                (
                    area.bottom() - 1,
                    if start + visible < rows.len() {
                        "↓"
                    } else {
                        " "
                    },
                ),
            ] {
                frame.render_widget(
                    Paragraph::new(glyph).style(Style::default().fg(theme.text_dim)),
                    Rect::new(area.x, y, 1, 1),
                );
            }
        }
    }

    pub(crate) fn render(
        &mut self,
        frame: &mut Frame,
        area: Rect,
        rows: &[SessionRow],
        active: Option<TerminalId>,
        portal: bool,
    ) {
        self.update(rows);
        let theme = crate::theme::current();
        self.panel = Rect::new(
            area.x,
            area.y,
            (area.width.saturating_mul(2) / 3)
                .clamp(1, 30)
                .min(area.width),
            // Cover only the rows this group needs; a small roster leaves
            // the running terminal visible underneath the short overlay.
            area.height.min(self.group_len().max(1) as u16 + 3),
        );
        if portal {
            self.panel = area;
        }
        self.items = Rect::default();
        if self.panel.is_empty() {
            return;
        }
        frame.render_widget(Clear, self.panel);
        let block = Block::default()
            .borders(if portal {
                Borders::empty()
            } else {
                Borders::RIGHT
            })
            .border_style(Style::default().fg(theme.chrome))
            .style(Style::default().bg(theme.surface));
        let inner = block.inner(self.panel);
        frame.render_widget(block, self.panel);
        if inner.is_empty() {
            return;
        }
        let heading = if self.is_prioritizing() {
            "Priority"
        } else {
            "Sessions"
        };
        let title = if self.targets.len() > LETTERS {
            format!(
                "{heading} {}/{} j/k",
                self.group + 1,
                self.targets.len().div_ceil(LETTERS)
            )
        } else {
            format!("{heading} j/k move")
        };
        frame.render_widget(
            Paragraph::new(title).style(Style::default().fg(theme.accent)),
            Rect::new(inner.x, inner.y, inner.width, 1),
        );
        self.items = Rect::new(
            inner.x,
            inner.y + 1,
            inner.width,
            inner.height.saturating_sub(3),
        );
        self.visible = usize::from(self.items.height);
        let cursor = self.cursor % LETTERS;
        self.scroll = self.scroll.min(cursor);
        if self.visible > 0 && cursor >= self.scroll + self.visible {
            self.scroll = cursor + 1 - self.visible;
        }
        self.scroll(0);
        if self.targets.is_empty() {
            frame.render_widget(
                Paragraph::new(
                    "No sessions yet.\nPress n to start one.\nA repository is optional.",
                )
                .style(Style::default().fg(theme.text_dim)),
                self.items,
            );
        }
        for (i, id) in self
            .targets
            .iter()
            .skip(self.group * LETTERS)
            .take(LETTERS)
            .enumerate()
            .skip(self.scroll)
            .take(self.visible)
        {
            let letter = char::from(SELECTORS[i]);
            let row = rows.iter().find(|r| r.terminal_id == *id);
            let (glyph, color) = row
                .map(SessionRow::indicator)
                .unwrap_or(("×", theme.text_dim));
            let name = row
                .map(|r| format!("{} · {}", r.title, r.runner))
                .unwrap_or_else(|| "session ended".into());
            let base = Style::default().bg(if Some(*id) == self.highlighted() {
                theme.fill
            } else {
                theme.surface
            });
            let line = Line::from(vec![
                Span::styled(glyph, base.fg(color)),
                Span::styled(
                    format!(" {letter} "),
                    base.fg(theme.accent).add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    crate::util::truncate_ellipsis(
                        &name,
                        usize::from(self.items.width.saturating_sub(4)),
                    )
                    .into_owned(),
                    base.fg(theme.text_strong)
                        .add_modifier(if Some(*id) == active {
                            Modifier::BOLD
                        } else {
                            Modifier::empty()
                        }),
                ),
            ]);
            frame.render_widget(
                Paragraph::new(line).style(base),
                Rect::new(
                    self.items.x,
                    self.items.y + (i - self.scroll) as u16,
                    self.items.width,
                    1,
                ),
            );
        }
        if inner.height >= 3
            && let Some(row) = rows
                .iter()
                .find(|r| Some(r.terminal_id) == self.highlighted())
        {
            frame.render_widget(
                Paragraph::new(if let Some(source) = self.priority {
                    format!(
                        "Move {}",
                        rows.iter()
                            .find(|r| r.terminal_id == source)
                            .map(|r| r.title.as_str())
                            .unwrap_or("ended session")
                    )
                } else {
                    row.detail.clone()
                })
                .style(Style::default().fg(if row.attention {
                    theme.warn
                } else {
                    theme.text_dim
                })),
                Rect::new(inner.x, inner.bottom() - 2, inner.width, 1),
            );
        }
        if inner.height > 1 {
            let hint = if self.is_prioritizing() {
                "Enter move Esc back"
            } else {
                "Enter open Esc back"
            }
            .to_string();
            let hint = if self.targets.len() > LETTERS {
                format!("{hint} [ ]")
            } else {
                hint
            };
            frame.render_widget(
                Paragraph::new(hint).style(Style::default().fg(theme.text_dim)),
                Rect::new(inner.x, inner.bottom() - 1, inner.width, 1),
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lazybox_core::SessionKey;
    use tuirealm::ratatui::{Terminal, backend::TestBackend};
    #[test]
    fn links_footer_is_visible_and_tappable_on_a_32_column_phone() {
        let mut rail = MobileRail::default();
        rail.open(&rows());
        assert!(rail.footer().len() <= 32);
        let start = rail.footer().find("/ URLs").unwrap();
        for col in start..rail.footer().len() {
            let key = rail.footer_key(col as u16).unwrap();
            assert!(matches!(rail.key(&key), RailAction::Links(TerminalId(1))));
        }
        rail.key(&KeyEvent::from(Key::Char('p')));
        assert!(matches!(
            rail.key(&KeyEvent::from(Key::Char('/'))),
            RailAction::None
        ));
    }

    fn rows() -> Vec<SessionRow> {
        (1..=60)
            .map(|id| SessionRow {
                terminal_id: TerminalId(id),
                session_key: SessionKey::new(format!("s-{id}")),
                title: format!("Chat {id}"),
                detail: String::new(),
                attention: false,
                runner: "shell".into(),
                state: None,
                exited: false,
            })
            .collect()
    }
    #[test]
    fn overflow_scroll_and_resize_preserve_letters_and_page_skips_reserved_keys() {
        let rows = rows();
        let mut rail = MobileRail::default();
        rail.open(&rows);
        for (w, h) in [(39, 16), (32, 10), (10, 4), (1, 1), (0, 0), (39, 16)] {
            let mut terminal = Terminal::new(TestBackend::new(w, h)).unwrap();
            terminal
                .draw(|f| rail.render(f, f.area(), &rows, Some(TerminalId(40)), false))
                .unwrap();
            rail.scroll(20);
            assert!(matches!(
                rail.key(&KeyEvent::from(Key::Char('b'))),
                RailAction::Select(TerminalId(2))
            ));
            terminal
                .draw(|f| rail.render(f, f.area(), &rows, Some(TerminalId(40)), false))
                .unwrap();
            if h >= 4 && w > 1 {
                assert_eq!(
                    rail.at(rail.items.x, rail.items.y),
                    Some(TerminalId(rail.scroll as u64 + 1))
                );
            }
            terminal
                .draw(|f| {
                    MobileRail::render_collapsed(
                        f,
                        Rect::new(0, 0, w.min(1), h),
                        &rows,
                        Some(TerminalId(40)),
                    )
                })
                .unwrap();
        }
        rail.key(&KeyEvent::from(Key::Char(']')));
        assert!(matches!(
            rail.key(&KeyEvent::from(Key::Char('a'))),
            RailAction::Select(TerminalId(21))
        ));
        rail.key(&KeyEvent::from(Key::Char('[')));
        assert!(matches!(
            rail.key(&KeyEvent::from(Key::Char('z'))),
            RailAction::Select(TerminalId(20))
        ));
    }
    #[test]
    fn priority_captures_source_and_destination_ids_and_can_cancel() {
        let mut rail = MobileRail::default();
        rail.open(&rows());
        rail.highlight_initial(TerminalId(40));
        assert!(matches!(
            rail.key(&KeyEvent::from(Key::Enter)),
            RailAction::Select(TerminalId(40))
        ));
        rail.key(&KeyEvent::from(Key::Char('p')));
        rail.key(&KeyEvent::from(Key::Char('[')));
        rail.key(&KeyEvent::from(Key::Char('j')));
        assert!(matches!(
            rail.key(&KeyEvent::from(Key::Enter)),
            RailAction::Prioritize {
                source: TerminalId(40),
                target: TerminalId(2)
            }
        ));
        assert!(!rail.is_prioritizing());
        rail.key(&KeyEvent::from(Key::Char('p')));
        rail.key(&KeyEvent::from(Key::Char('j')));
        // Pressing p again must not replace the captured source.
        rail.key(&KeyEvent::from(Key::Char('p')));
        assert!(matches!(
            rail.key(&KeyEvent::from(Key::Char('a'))),
            RailAction::Prioritize {
                source: TerminalId(2),
                target: TerminalId(1)
            }
        ));
        rail.key(&KeyEvent::from(Key::Char('p')));
        assert!(matches!(
            rail.key(&KeyEvent::from(Key::Esc)),
            RailAction::None
        ));
        assert!(!rail.is_prioritizing());
        assert!(rail.is_open());
        assert!(matches!(
            rail.key(&KeyEvent::from(Key::Esc)),
            RailAction::Close
        ));
        rail.open(&[]);
        rail.key(&KeyEvent::from(Key::Char('p')));
        assert!(!rail.is_prioritizing());
        assert!(matches!(
            rail.key(&KeyEvent::from(Key::Enter)),
            RailAction::None
        ));
    }

    #[test]
    fn jk_moves_highlight_without_selecting_a_session() {
        let mut rail = MobileRail::default();
        rail.open(&rows());
        assert!(matches!(
            rail.key(&KeyEvent::from(Key::Char('j'))),
            RailAction::None
        ));
        assert_eq!(rail.highlighted(), Some(TerminalId(2)));
        assert!(matches!(
            rail.key(&KeyEvent::from(Key::Char('k'))),
            RailAction::None
        ));
        assert_eq!(rail.highlighted(), Some(TerminalId(1)));
    }
    #[test]
    fn selector_map_reserves_j_k_n_p_r_x_in_every_group() {
        let mut rail = MobileRail::default();
        rail.open(&rows());
        for (i, c) in "abcdefghilmoqstuvwyz".chars().enumerate() {
            assert!(
                matches!(rail.key(&KeyEvent::from(Key::Char(c))), RailAction::Select(TerminalId(id)) if id == i as u64 + 1)
            );
        }
        assert!(matches!(
            rail.key(&KeyEvent::from(Key::Char('n'))),
            RailAction::New
        ));
        assert!(matches!(
            rail.key(&KeyEvent::from(Key::Char('r'))),
            RailAction::Rename(TerminalId(1))
        ));
        assert!(matches!(
            rail.key(&KeyEvent::from(Key::Char('x'))),
            RailAction::Delete(TerminalId(1))
        ));
        rail.key(&KeyEvent::from(Key::Down));
        assert!(matches!(
            rail.key(&KeyEvent::from(Key::Char('r'))),
            RailAction::Rename(TerminalId(2))
        ));
        rail.key(&KeyEvent::from(Key::Char(']')));
        for (i, c) in "abcdefghilmoqstuvwyz".chars().enumerate() {
            assert!(
                matches!(rail.key(&KeyEvent::from(Key::Char(c))), RailAction::Select(TerminalId(id)) if id == i as u64 + 21)
            );
        }
        assert!(matches!(
            rail.key(&KeyEvent::from(Key::Enter)),
            RailAction::Select(TerminalId(21))
        ));
        assert!(matches!(
            rail.key(&KeyEvent::new(Key::Char('q'), KeyModifiers::CONTROL)),
            RailAction::Quit
        ));
    }
}
