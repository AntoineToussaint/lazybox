//! Structured line editor for the personal Hopper.
//!
//! Unlike a plain textarea, every existing line retains its WorkspaceKey
//! while it is renamed or reordered. Saving therefore cannot confuse text
//! equality with workspace identity.

use crate::realm::{Msg, UserEvent};
use lazybox_core::WorkspaceKey;
use lazybox_ipc::HopperEntryDraft;
use tuirealm::command::{Cmd, CmdResult};
use tuirealm::component::{AppComponent, Component};
use tuirealm::event::{Event, Key, KeyModifiers};
use tuirealm::props::{AttrValue, Attribute, QueryResult};
use tuirealm::ratatui::layout::Rect;
use tuirealm::ratatui::prelude::*;
use tuirealm::ratatui::widgets::{Block, BorderType, Borders, Clear, Paragraph};
use tuirealm::state::State;

#[derive(Debug, Clone, PartialEq, Eq)]
struct Row {
    key: Option<WorkspaceKey>,
    name: String,
}

/// Modal editor for an ordered set of Hopper workspaces.
pub struct HopperEditor {
    rows: Vec<Row>,
    row: usize,
    cursor: usize,
    error: Option<String>,
}

impl HopperEditor {
    /// Build the editor from active hopper rows in display order.
    pub fn new(rows: Vec<(WorkspaceKey, String)>) -> Self {
        let mut rows: Vec<Row> = rows
            .into_iter()
            .map(|(key, name)| Row {
                key: Some(key),
                name,
            })
            .collect();
        rows.push(Row {
            key: None,
            name: String::new(),
        });
        let row = rows.len() - 1;
        Self {
            rows,
            row,
            cursor: 0,
            error: None,
        }
    }

    fn current(&self) -> &Row {
        &self.rows[self.row]
    }

    fn current_mut(&mut self) -> &mut Row {
        &mut self.rows[self.row]
    }

    fn move_left(&mut self) {
        if self.cursor == 0 {
            return;
        }
        let last = self.current().name[..self.cursor]
            .chars()
            .next_back()
            .expect("cursor is after one character");
        self.cursor -= last.len_utf8();
    }

    fn move_right(&mut self) {
        if self.cursor >= self.current().name.len() {
            return;
        }
        let next = self.current().name[self.cursor..]
            .chars()
            .next()
            .expect("cursor is before one character");
        self.cursor += next.len_utf8();
    }

    fn move_vertical(&mut self, delta: isize) {
        let column = self.current().name[..self.cursor].chars().count();
        let target = self
            .row
            .saturating_add_signed(delta)
            .min(self.rows.len().saturating_sub(1));
        self.row = target;
        self.cursor = char_column_to_byte(&self.current().name, column);
        self.error = None;
    }

    fn insert_char(&mut self, ch: char) {
        let cursor = self.cursor;
        self.current_mut().name.insert(cursor, ch);
        self.cursor += ch.len_utf8();
        self.error = None;
    }

    fn insert_row_break(&mut self) {
        let cursor = self.cursor;
        let suffix = self.current_mut().name.split_off(cursor);
        self.rows.insert(
            self.row + 1,
            Row {
                key: None,
                name: suffix,
            },
        );
        self.row += 1;
        self.cursor = 0;
        self.error = None;
    }

    fn backspace(&mut self) {
        if self.cursor > 0 {
            let before = &self.current().name[..self.cursor];
            let ch = before
                .chars()
                .next_back()
                .expect("cursor is after one character");
            let start = self.cursor - ch.len_utf8();
            let cursor = self.cursor;
            self.current_mut().name.replace_range(start..cursor, "");
            self.cursor = start;
            self.error = None;
            return;
        }
        if self.row == 0 {
            return;
        }
        if self.current().key.is_some() {
            self.error = Some("Use complete or delete for an existing item".into());
            return;
        }
        let removed = self.rows.remove(self.row);
        self.row -= 1;
        self.cursor = self.current().name.len();
        self.current_mut().name.push_str(&removed.name);
        self.error = None;
    }

    fn drafts(&mut self) -> Option<Vec<HopperEntryDraft>> {
        if self
            .rows
            .iter()
            .any(|row| row.key.is_some() && row.name.trim().is_empty())
        {
            self.error =
                Some("Existing items need a title; complete or delete them instead".into());
            return None;
        }
        Some(
            self.rows
                .iter()
                .filter(|row| !row.name.trim().is_empty())
                .map(|row| HopperEntryDraft {
                    workspace_key: row.key.clone(),
                    name: row.name.trim().to_string(),
                })
                .collect(),
        )
    }
}

fn char_column_to_byte(value: &str, column: usize) -> usize {
    value
        .char_indices()
        .nth(column)
        .map(|(idx, _)| idx)
        .unwrap_or(value.len())
}

impl Component for HopperEditor {
    fn view(&mut self, frame: &mut Frame, area: Rect) {
        let theme = crate::theme::current();
        let width = 80u16.min(area.width.saturating_sub(4));
        let height = 24u16.min(area.height.saturating_sub(4));
        let modal = Rect::new(
            area.x + area.width.saturating_sub(width) / 2,
            area.y + area.height.saturating_sub(height) / 2,
            width,
            height,
        );
        frame.render_widget(Clear, modal);
        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .title(Span::styled(" Hopper ", theme.modal_title()))
            .border_style(theme.modal_border());
        let inner = block.inner(modal);
        frame.render_widget(block, modal);

        let body_height = inner.height.saturating_sub(3) as usize;
        let start = self
            .row
            .saturating_sub(body_height.saturating_sub(1))
            .min(self.rows.len().saturating_sub(body_height));
        let lines = self
            .rows
            .iter()
            .enumerate()
            .skip(start)
            .take(body_height)
            .map(|(index, row)| {
                let selected = index == self.row;
                let prefix = if selected { "> " } else { "  " };
                let style = if selected {
                    theme.row_focused()
                } else {
                    Style::default().fg(theme.text_strong)
                };
                if selected {
                    let col = self.cursor.min(row.name.len());
                    let before = row.name[..col].to_string();
                    let after = row.name[col..].to_string();
                    Line::from(vec![
                        Span::styled(prefix, style),
                        Span::styled(before, style),
                        Span::styled("▌", style.fg(theme.accent)),
                        Span::styled(after, style),
                    ])
                } else {
                    Line::from(Span::styled(format!("{prefix}{}", row.name), style))
                }
            })
            .collect::<Vec<_>>();
        frame.render_widget(
            Paragraph::new(lines),
            Rect::new(inner.x, inner.y, inner.width, body_height as u16),
        );

        let mut help = vec![
            Span::styled("Enter", Style::default().fg(theme.success).bold()),
            Span::raw(" next item  "),
            Span::styled("↑↓", Style::default().fg(theme.text_dim).bold()),
            Span::raw(" move  "),
            Span::styled("Ctrl-S", Style::default().fg(theme.success).bold()),
            Span::raw(" save  "),
            Span::styled("Esc", Style::default().fg(theme.error).bold()),
            Span::raw(" cancel"),
        ];
        if let Some(error) = &self.error {
            help.push(Span::raw("  "));
            help.push(Span::styled(
                error.clone(),
                Style::default().fg(theme.error),
            ));
        }
        frame.render_widget(
            Paragraph::new(vec![
                Line::from(Span::styled(
                    "One line becomes one persistent workspace. Paste a list to capture in bulk.",
                    Style::default().fg(theme.text_dim),
                )),
                Line::from(help),
            ]),
            Rect::new(
                inner.x,
                inner.y + body_height as u16,
                inner.width,
                inner.height.saturating_sub(body_height as u16),
            ),
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

impl AppComponent<Msg, UserEvent> for HopperEditor {
    fn on(&mut self, event: &Event<UserEvent>) -> Option<Msg> {
        if let Event::Paste(text) = event {
            for ch in text.chars() {
                if ch == '\n' {
                    self.insert_row_break();
                } else if !ch.is_control() {
                    self.insert_char(ch);
                }
            }
            return None;
        }
        let Event::Keyboard(key) = event else {
            return None;
        };
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        if matches!(key.code, Key::Esc) || (ctrl && matches!(key.code, Key::Char('c'))) {
            return Some(Msg::ModalDismissed);
        }
        if ctrl && matches!(key.code, Key::Char('s')) {
            return self.drafts().map(Msg::HopperSubmitted);
        }
        match key.code {
            Key::Enter => self.insert_row_break(),
            Key::Up => self.move_vertical(-1),
            Key::Down => self.move_vertical(1),
            Key::Left => self.move_left(),
            Key::Right => self.move_right(),
            Key::Home => self.cursor = 0,
            Key::End => self.cursor = self.current().name.len(),
            Key::Backspace => self.backspace(),
            Key::Char(ch) if !ctrl => self.insert_char(ch),
            _ => return None,
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tuirealm::event::KeyEvent;

    fn key(code: Key) -> Event<UserEvent> {
        Event::Keyboard(KeyEvent {
            code,
            modifiers: KeyModifiers::NONE,
        })
    }

    #[test]
    fn enter_creates_a_new_identity_without_losing_the_existing_one() {
        let existing = WorkspaceKey::new("first");
        let mut editor = HopperEditor::new(vec![(existing.clone(), "First".into())]);
        editor.on(&key(Key::Up));
        editor.on(&key(Key::End));
        editor.on(&key(Key::Enter));
        editor.on(&key(Key::Char('N')));
        let drafts = editor.drafts().unwrap();
        assert_eq!(drafts[0].workspace_key, Some(existing));
        assert_eq!(drafts[1].workspace_key, None);
        assert_eq!(drafts[1].name, "N");
    }

    #[test]
    fn existing_row_cannot_be_erased_into_an_implicit_delete() {
        let mut editor = HopperEditor::new(vec![(WorkspaceKey::new("first"), "A".into())]);
        editor.on(&key(Key::Up));
        editor.on(&key(Key::End));
        editor.on(&key(Key::Backspace));
        assert!(editor.drafts().is_none());
        assert!(
            editor
                .error
                .as_deref()
                .unwrap()
                .contains("complete or delete")
        );
    }
}
