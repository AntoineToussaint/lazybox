//! Structured line editor and dated history for the personal Hopper.
//!
//! Unlike a plain textarea, every existing line retains its WorkspaceKey
//! while it is renamed or reordered. Lifecycle actions update one row in
//! place, while destructive deletion remains a separate explicit chord.

use crate::realm::{Msg, UserEvent};
use chrono::{DateTime, Local, NaiveDate, Utc};
use lazybox_core::WorkspaceKey;
use lazybox_ipc::HopperEntryDraft;
use std::collections::BTreeSet;
use tuirealm::command::{Cmd, CmdResult};
use tuirealm::component::{AppComponent, Component};
use tuirealm::event::{Event, Key, KeyModifiers};
use tuirealm::props::{AttrValue, Attribute, QueryResult};
use tuirealm::ratatui::layout::Rect;
use tuirealm::ratatui::prelude::*;
use tuirealm::ratatui::widgets::{Block, BorderType, Borders, Clear, Paragraph, Wrap};
use tuirealm::state::State;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct HopperItem {
    pub(crate) key: WorkspaceKey,
    pub(crate) name: String,
    pub(crate) created_at: DateTime<Utc>,
    pub(crate) completed_at: Option<DateTime<Utc>>,
    pub(crate) canceled_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Row {
    key: Option<WorkspaceKey>,
    name: String,
    created_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Outcome {
    Done,
    Canceled,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct HistoryItem {
    key: WorkspaceKey,
    name: String,
    created_at: DateTime<Utc>,
    outcome_at: DateTime<Utc>,
    outcome: Outcome,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HopperTab {
    Active,
    History,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HistoryTarget {
    Day(NaiveDate),
    Item(usize),
}

/// Modal editor for an ordered set of Hopper workspaces.
pub struct HopperEditor {
    rows: Vec<Row>,
    history: Vec<HistoryItem>,
    tab: HopperTab,
    row: usize,
    cursor: usize,
    history_cursor: usize,
    expanded_days: BTreeSet<NaiveDate>,
    error: Option<String>,
}

impl HopperEditor {
    /// Build the editor from all Hopper workspaces. Active items remain
    /// editable; completed and canceled items move into dated history.
    pub(crate) fn new(items: Vec<HopperItem>) -> Self {
        let mut rows = Vec::new();
        let mut history = Vec::new();
        for item in items {
            let outcome = match (item.completed_at, item.canceled_at) {
                (Some(at), _) => Some((Outcome::Done, at)),
                (None, Some(at)) => Some((Outcome::Canceled, at)),
                (None, None) => None,
            };
            if let Some((outcome, outcome_at)) = outcome {
                history.push(HistoryItem {
                    key: item.key,
                    name: item.name,
                    created_at: item.created_at,
                    outcome_at,
                    outcome,
                });
            } else {
                rows.push(Row {
                    key: Some(item.key),
                    name: item.name,
                    created_at: Some(item.created_at),
                });
            }
        }
        rows.push(Self::blank_row());
        let row = rows.len() - 1;
        Self {
            rows,
            history,
            tab: HopperTab::Active,
            row,
            cursor: 0,
            history_cursor: 0,
            expanded_days: BTreeSet::new(),
            error: None,
        }
    }

    fn blank_row() -> Row {
        Row {
            key: None,
            name: String::new(),
            created_at: None,
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
        self.row = self
            .row
            .saturating_add_signed(delta)
            .min(self.rows.len().saturating_sub(1));
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
                created_at: None,
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
            self.error = Some("Use Ctrl-X to cancel or Ctrl-K to delete this item".into());
            return;
        }
        let removed = self.rows.remove(self.row);
        self.row -= 1;
        self.cursor = self.current().name.len();
        self.current_mut().name.push_str(&removed.name);
        self.error = None;
    }

    fn delete_forward(&mut self) {
        if self.cursor >= self.current().name.len() {
            return;
        }
        let next = self.current().name[self.cursor..]
            .chars()
            .next()
            .expect("cursor is before one character");
        let start = self.cursor;
        self.current_mut()
            .name
            .replace_range(start..start + next.len_utf8(), "");
        self.error = None;
    }

    fn delete_current_line(&mut self) -> Option<Msg> {
        if self.rows.len() == 1 && self.current().name.is_empty() {
            return None;
        }
        let removed = self.rows.remove(self.row);
        if self.rows.is_empty() || self.rows.last().is_some_and(|row| row.key.is_some()) {
            self.rows.push(Self::blank_row());
        }
        self.row = self.row.min(self.rows.len().saturating_sub(1));
        self.cursor = self.current().name.len();
        self.error = None;
        removed.key.map(Msg::HopperDeleteRequested)
    }

    fn move_current_to_history(&mut self, outcome: Outcome) -> Option<Msg> {
        let row = self.current().clone();
        let Some(key) = row.key else {
            self.error = Some("Save this new item before changing its status".into());
            return None;
        };
        let now = Utc::now();
        self.rows.remove(self.row);
        if self.rows.is_empty() || self.rows.last().is_some_and(|row| row.key.is_some()) {
            self.rows.push(Self::blank_row());
        }
        self.row = self.row.min(self.rows.len().saturating_sub(1));
        self.cursor = self.current().name.len();
        self.history.push(HistoryItem {
            key: key.clone(),
            name: row.name,
            created_at: row.created_at.unwrap_or(now),
            outcome_at: now,
            outcome,
        });
        self.error = None;
        Some(match outcome {
            Outcome::Done => Msg::HopperCompletionRequested {
                workspace_key: key,
                completed: true,
            },
            Outcome::Canceled => Msg::HopperCancellationRequested {
                workspace_key: key,
                canceled: true,
            },
        })
    }

    fn drafts(&mut self) -> Option<Vec<HopperEntryDraft>> {
        if self
            .rows
            .iter()
            .any(|row| row.key.is_some() && row.name.trim().is_empty())
        {
            self.error = Some("Existing items need a title; cancel or delete them instead".into());
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

    fn history_dates(&self) -> Vec<NaiveDate> {
        let mut dates: Vec<_> = self
            .history
            .iter()
            .map(|item| item.outcome_at.with_timezone(&Local).date_naive())
            .collect();
        dates.sort_unstable_by(|a, b| b.cmp(a));
        dates.dedup();
        dates
    }

    fn history_indices_for_date(&self, date: NaiveDate) -> Vec<usize> {
        let mut items: Vec<_> = self
            .history
            .iter()
            .enumerate()
            .filter(|(_, item)| item.outcome_at.with_timezone(&Local).date_naive() == date)
            .map(|(index, _)| index)
            .collect();
        items.sort_by_key(|index| {
            let item = &self.history[*index];
            (item.outcome, item.outcome_at)
        });
        items
    }

    fn history_targets(&self) -> Vec<HistoryTarget> {
        let mut targets = Vec::new();
        for date in self.history_dates() {
            targets.push(HistoryTarget::Day(date));
            if self.expanded_days.contains(&date) {
                targets.extend(
                    self.history_indices_for_date(date)
                        .into_iter()
                        .map(HistoryTarget::Item),
                );
            }
        }
        targets
    }

    fn toggle_history_day(&mut self) {
        let Some(HistoryTarget::Day(date)) =
            self.history_targets().get(self.history_cursor).copied()
        else {
            return;
        };
        if !self.expanded_days.remove(&date) {
            self.expanded_days.insert(date);
        }
    }

    fn move_history_day(&mut self, delta: isize) {
        let last = self.history_targets().len().saturating_sub(1);
        self.history_cursor = self.history_cursor.saturating_add_signed(delta).min(last);
    }

    fn reopen_history_item(&mut self) -> Option<Msg> {
        let Some(HistoryTarget::Item(index)) =
            self.history_targets().get(self.history_cursor).copied()
        else {
            self.error = Some("Expand a day and select an item to reopen it".into());
            return None;
        };
        let item = self.history.remove(index);
        let outcome = item.outcome;
        let key = item.key.clone();
        let insert_at = self.rows.len().saturating_sub(1);
        self.rows.insert(
            insert_at,
            Row {
                key: Some(item.key),
                name: item.name,
                created_at: Some(item.created_at),
            },
        );
        self.history_cursor = self
            .history_cursor
            .min(self.history_targets().len().saturating_sub(1));
        self.error = None;
        Some(match outcome {
            Outcome::Done => Msg::HopperCompletionRequested {
                workspace_key: key,
                completed: false,
            },
            Outcome::Canceled => Msg::HopperCancellationRequested {
                workspace_key: key,
                canceled: false,
            },
        })
    }

    fn render_active(&self, frame: &mut Frame, area: Rect, theme: &crate::theme::Theme) {
        let body_height = area.height as usize;
        let start = self
            .row
            .saturating_sub(body_height.saturating_sub(1))
            .min(self.rows.len().saturating_sub(body_height));
        let text_width = area.width.saturating_sub(7) as usize;
        let lines = self
            .rows
            .iter()
            .enumerate()
            .skip(start)
            .take(body_height)
            .map(|(index, row)| {
                let selected = index == self.row;
                let pointer = if selected { "> " } else { "  " };
                let style = if selected {
                    theme.row_focused()
                } else {
                    Style::default().fg(theme.text_strong)
                };
                if selected {
                    let (before, after) = cursor_window(&row.name, self.cursor, text_width);
                    Line::from(vec![
                        Span::styled(pointer, style),
                        Span::styled("[ ] ", style.fg(theme.text_dim)),
                        Span::styled(before, style),
                        Span::styled("▌", style.fg(theme.accent)),
                        Span::styled(after, style),
                    ])
                } else {
                    let name = crate::util::truncate_ellipsis(&row.name, text_width);
                    Line::from(Span::styled(format!("{pointer}[ ] {name}"), style))
                }
            })
            .collect::<Vec<_>>();
        frame.render_widget(Paragraph::new(lines), area);
    }

    fn render_history(&self, frame: &mut Frame, area: Rect, theme: &crate::theme::Theme) {
        let dates = self.history_dates();
        if dates.is_empty() {
            frame.render_widget(
                Paragraph::new("No completed or canceled items yet.")
                    .style(Style::default().fg(theme.text_dim)),
                area,
            );
            return;
        }
        let targets = self.history_targets();
        let lines = targets
            .iter()
            .enumerate()
            .map(|(line_index, target)| {
                let selected = line_index == self.history_cursor;
                let style = if selected {
                    theme.row_focused()
                } else {
                    Style::default().fg(theme.text_strong)
                };
                match *target {
                    HistoryTarget::Day(date) => {
                        let items = self.history_indices_for_date(date);
                        let done = items
                            .iter()
                            .filter(|index| self.history[**index].outcome == Outcome::Done)
                            .count();
                        let canceled = items.len() - done;
                        let pointer = if selected { ">" } else { " " };
                        let disclosure = if self.expanded_days.contains(&date) {
                            "▾"
                        } else {
                            "▸"
                        };
                        Line::from(vec![
                            Span::styled(format!("{pointer} {disclosure} {date}"), style),
                            Span::styled(
                                format!("  {done} done · {canceled} canceled"),
                                style.fg(theme.text_dim),
                            ),
                        ])
                    }
                    HistoryTarget::Item(index) => {
                        let item = &self.history[index];
                        let outcome = match item.outcome {
                            Outcome::Done => "✓ done",
                            Outcome::Canceled => "× canceled",
                        };
                        let created = item.created_at.with_timezone(&Local).format("%H:%M");
                        let ended = item.outcome_at.with_timezone(&Local).format("%H:%M");
                        let name = crate::util::truncate_ellipsis(
                            &item.name,
                            (area.width as usize).saturating_sub(36),
                        );
                        let pointer = if selected { ">" } else { " " };
                        Line::from(vec![
                            Span::styled(format!("{pointer}   "), style),
                            Span::styled(
                                format!("{outcome:<10}"),
                                style.fg(if item.outcome == Outcome::Done {
                                    theme.success
                                } else {
                                    theme.text_dim
                                }),
                            ),
                            Span::styled(name, style),
                            Span::styled(
                                format!("  made {created} · {ended}"),
                                style.fg(theme.text_dim),
                            ),
                        ])
                    }
                }
            })
            .collect::<Vec<_>>();
        let scroll = self
            .history_cursor
            .saturating_sub((area.height as usize).saturating_sub(1))
            .min(u16::MAX as usize);
        frame.render_widget(Paragraph::new(lines).scroll((scroll as u16, 0)), area);
    }
}

fn char_column_to_byte(value: &str, column: usize) -> usize {
    value
        .char_indices()
        .nth(column)
        .map(|(idx, _)| idx)
        .unwrap_or(value.len())
}

fn cursor_window(value: &str, cursor: usize, width: usize) -> (String, String) {
    if width == 0 {
        return (String::new(), String::new());
    }
    let cursor_chars = value[..cursor.min(value.len())].chars().count();
    let chars: Vec<char> = value.chars().collect();
    let start = cursor_chars.saturating_sub(width.saturating_sub(1));
    let end = (start + width).min(chars.len());
    let mut before: String = chars[start..cursor_chars.min(end)].iter().collect();
    let mut after: String = chars[cursor_chars.min(end)..end].iter().collect();
    if start > 0 {
        before.insert(0, '…');
    }
    if end < chars.len() {
        after.push('…');
    }
    (before, after)
}

impl Component for HopperEditor {
    fn view(&mut self, frame: &mut Frame, area: Rect) {
        let theme = crate::theme::current();
        let width = 90u16.min(area.width.saturating_sub(4));
        let height = 28u16.min(area.height.saturating_sub(4));
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

        let active_count = self
            .rows
            .iter()
            .filter(|row| row.key.is_some() || !row.name.trim().is_empty())
            .count();
        let tabs = Line::from(vec![
            Span::styled(
                format!(" Active {active_count} "),
                if self.tab == HopperTab::Active {
                    theme.row_focused()
                } else {
                    Style::default().fg(theme.text_dim)
                },
            ),
            Span::raw("  "),
            Span::styled(
                format!(" History {} ", self.history.len()),
                if self.tab == HopperTab::History {
                    theme.row_focused()
                } else {
                    Style::default().fg(theme.text_dim)
                },
            ),
            Span::styled("    Tab switch", Style::default().fg(theme.text_dim)),
        ]);
        frame.render_widget(
            Paragraph::new(tabs),
            Rect::new(inner.x, inner.y, inner.width, 1),
        );

        let help_height = 4u16.min(inner.height.saturating_sub(2));
        let body = Rect::new(
            inner.x,
            inner.y + 2,
            inner.width,
            inner.height.saturating_sub(help_height + 2),
        );
        match self.tab {
            HopperTab::Active => self.render_active(frame, body, theme),
            HopperTab::History => self.render_history(frame, body, theme),
        }

        let mut help = match self.tab {
            HopperTab::Active => vec![
                Line::from(vec![
                    Span::styled("Ctrl-D", Style::default().fg(theme.success).bold()),
                    Span::raw(" done  "),
                    Span::styled("Ctrl-X", Style::default().fg(theme.text_dim).bold()),
                    Span::raw(" cancel  "),
                    Span::styled("Ctrl-K", Style::default().fg(theme.error).bold()),
                    Span::raw(" delete line"),
                ]),
                Line::from(vec![
                    Span::styled("Enter", Style::default().fg(theme.success).bold()),
                    Span::raw(" next item  "),
                    Span::styled("↑↓", Style::default().fg(theme.text_dim).bold()),
                    Span::raw(" move  "),
                    Span::styled("Ctrl-S", Style::default().fg(theme.success).bold()),
                    Span::raw(" save  "),
                    Span::styled("Esc", Style::default().fg(theme.error).bold()),
                    Span::raw(" close"),
                ]),
                Line::from(Span::styled(
                    "One line is one persistent workspace. Paste newline-separated items to capture in bulk.",
                    Style::default().fg(theme.text_dim),
                )),
            ],
            HopperTab::History => vec![
                Line::from(vec![
                    Span::styled("↑↓", Style::default().fg(theme.text_dim).bold()),
                    Span::raw(" day  "),
                    Span::styled("Space", Style::default().fg(theme.success).bold()),
                    Span::raw(" expand/collapse  "),
                    Span::styled("r", Style::default().fg(theme.success).bold()),
                    Span::raw(" reopen item  "),
                    Span::styled("Tab", Style::default().fg(theme.success).bold()),
                    Span::raw(" active  "),
                    Span::styled("Esc", Style::default().fg(theme.error).bold()),
                    Span::raw(" close"),
                ]),
                Line::from(Span::styled(
                    "Completed items are listed before canceled items within each day.",
                    Style::default().fg(theme.text_dim),
                )),
            ],
        };
        if let Some(error) = &self.error {
            help.push(Line::from(Span::styled(
                error.clone(),
                Style::default().fg(theme.error),
            )));
        }
        frame.render_widget(
            Paragraph::new(help).wrap(Wrap { trim: false }),
            Rect::new(
                inner.x,
                inner.y + inner.height.saturating_sub(help_height),
                inner.width,
                help_height,
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
            if self.tab == HopperTab::History {
                return None;
            }
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
        let shift = key.modifiers.contains(KeyModifiers::SHIFT);
        if matches!(key.code, Key::Esc) || (ctrl && matches!(key.code, Key::Char('c'))) {
            return Some(Msg::ModalDismissed);
        }
        if matches!(key.code, Key::Tab | Key::BackTab) {
            self.tab = match self.tab {
                HopperTab::Active => HopperTab::History,
                HopperTab::History => HopperTab::Active,
            };
            self.error = None;
            return None;
        }
        if self.tab == HopperTab::History {
            match key.code {
                Key::Up => self.move_history_day(-1),
                Key::Down => self.move_history_day(1),
                Key::Char(' ') | Key::Enter => self.toggle_history_day(),
                Key::Char('r') => return self.reopen_history_item(),
                _ => return None,
            }
            return None;
        }
        if ctrl && matches!(key.code, Key::Char('s')) {
            return self.drafts().map(Msg::HopperSubmitted);
        }
        if ctrl && matches!(key.code, Key::Char('d')) {
            return self.move_current_to_history(Outcome::Done);
        }
        // Cancel and delete need chords every terminal can deliver.
        // Ctrl+letter is the only reliable command idiom inside a text
        // editor (like Ctrl-S / Ctrl-D above): many emulators send plain
        // 0x7f/0x08 for Backspace with no modifier bit, so a
        // modifier+Backspace combo is unreachable there. Ctrl-X (cancel)
        // and Ctrl-K (delete line) are the primary paths; the
        // modifier+Delete/Backspace combos below are convenience aliases
        // for terminals that do report them.
        if ctrl && matches!(key.code, Key::Char('x')) {
            return self.move_current_to_history(Outcome::Canceled);
        }
        if ctrl && matches!(key.code, Key::Char('k')) {
            return self.delete_current_line();
        }
        if ctrl && matches!(key.code, Key::Delete | Key::Backspace) {
            return self.delete_current_line();
        }
        if shift && matches!(key.code, Key::Delete | Key::Backspace) {
            return self.move_current_to_history(Outcome::Canceled);
        }
        match key.code {
            Key::Delete => self.delete_forward(),
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

    fn item(name: &str) -> HopperItem {
        HopperItem {
            key: WorkspaceKey::new(name.to_lowercase()),
            name: name.into(),
            created_at: Utc::now(),
            completed_at: None,
            canceled_at: None,
        }
    }

    fn key(code: Key) -> Event<UserEvent> {
        Event::Keyboard(KeyEvent {
            code,
            modifiers: KeyModifiers::NONE,
        })
    }

    fn modified(code: Key, modifiers: KeyModifiers) -> Event<UserEvent> {
        Event::Keyboard(KeyEvent { code, modifiers })
    }

    fn render(editor: &mut HopperEditor, width: u16, height: u16) -> String {
        use tuirealm::ratatui::Terminal;
        use tuirealm::ratatui::backend::TestBackend;
        let mut terminal = Terminal::new(TestBackend::new(width, height)).expect("test terminal");
        terminal
            .draw(|frame| editor.view(frame, Rect::new(0, 0, width, height)))
            .expect("render Hopper");
        let buffer = terminal.backend().buffer();
        (0..buffer.area.height)
            .map(|y| {
                let mut row = String::new();
                for x in 0..buffer.area.width {
                    row.push_str(buffer[(x, y)].symbol());
                }
                row.trim_end().to_string()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn enter_creates_a_new_identity_without_losing_the_existing_one() {
        let existing = item("First");
        let existing_key = existing.key.clone();
        let mut editor = HopperEditor::new(vec![existing]);
        editor.on(&key(Key::Up));
        editor.on(&key(Key::End));
        editor.on(&key(Key::Enter));
        editor.on(&key(Key::Char('N')));
        let drafts = editor.drafts().expect("valid drafts");
        assert_eq!(drafts[0].workspace_key, Some(existing_key));
        assert_eq!(drafts[1].workspace_key, None);
        assert_eq!(drafts[1].name, "N");
    }

    #[test]
    fn done_and_cancel_are_in_place_and_accumulate_in_history() {
        let first = item("First");
        let second = item("Second");
        let first_key = first.key.clone();
        let second_key = second.key.clone();
        let mut editor = HopperEditor::new(vec![first, second]);
        editor.on(&key(Key::Up));
        editor.on(&key(Key::Up));
        assert_eq!(
            editor.on(&modified(Key::Char('d'), KeyModifiers::CONTROL)),
            Some(Msg::HopperCompletionRequested {
                workspace_key: first_key,
                completed: true,
            })
        );
        assert_eq!(
            editor.on(&modified(Key::Backspace, KeyModifiers::SHIFT)),
            Some(Msg::HopperCancellationRequested {
                workspace_key: second_key,
                canceled: true,
            })
        );
        assert_eq!(editor.history.len(), 2);
        assert_eq!(editor.rows.len(), 1, "only the capture row remains");
    }

    #[test]
    fn shifted_delete_and_backspace_cancel_into_history() {
        for code in [Key::Delete, Key::Backspace] {
            let existing = item("First");
            let existing_key = existing.key.clone();
            let mut editor = HopperEditor::new(vec![existing]);
            editor.on(&key(Key::Up));
            assert_eq!(
                editor.on(&modified(code, KeyModifiers::SHIFT)),
                Some(Msg::HopperCancellationRequested {
                    workspace_key: existing_key,
                    canceled: true,
                })
            );
            assert_eq!(editor.history.len(), 1);
            assert_eq!(editor.history[0].outcome, Outcome::Canceled);
        }
    }

    #[test]
    fn ctrl_x_cancels_into_history_on_every_terminal() {
        // Ctrl+letter is the reliable primary; the Shift-Backspace alias
        // is unreachable on emulators that can't report a Backspace
        // modifier, so this path must stand on its own.
        let existing = item("First");
        let existing_key = existing.key.clone();
        let mut editor = HopperEditor::new(vec![existing]);
        editor.on(&key(Key::Up));
        assert_eq!(
            editor.on(&modified(Key::Char('x'), KeyModifiers::CONTROL)),
            Some(Msg::HopperCancellationRequested {
                workspace_key: existing_key,
                canceled: true,
            })
        );
        assert_eq!(editor.history.len(), 1);
        assert_eq!(editor.history[0].outcome, Outcome::Canceled);
    }

    #[test]
    fn ctrl_k_deletes_whole_line_on_every_terminal() {
        let existing = item("First");
        let existing_key = existing.key.clone();
        let mut editor = HopperEditor::new(vec![existing]);
        editor.on(&key(Key::Up));
        assert_eq!(
            editor.on(&modified(Key::Char('k'), KeyModifiers::CONTROL)),
            Some(Msg::HopperDeleteRequested(existing_key))
        );
        assert!(editor.history.is_empty());
        assert_eq!(editor.rows.len(), 1);
    }

    #[test]
    fn controlled_delete_and_backspace_remove_whole_lines_not_history() {
        for code in [Key::Delete, Key::Backspace] {
            let existing = item("First");
            let existing_key = existing.key.clone();
            let mut editor = HopperEditor::new(vec![existing]);
            editor.on(&key(Key::Up));
            assert_eq!(
                editor.on(&modified(code, KeyModifiers::CONTROL)),
                Some(Msg::HopperDeleteRequested(existing_key))
            );
            assert!(editor.history.is_empty());
            assert_eq!(editor.rows.len(), 1);
        }
    }

    #[test]
    fn plain_delete_remains_a_text_editing_key() {
        let existing = item("First");
        let existing_key = existing.key.clone();
        let mut editor = HopperEditor::new(vec![existing]);
        editor.on(&key(Key::Up));
        assert_eq!(editor.on(&key(Key::Delete)), None);
        let drafts = editor.drafts().expect("valid drafts");
        assert_eq!(drafts[0].workspace_key, Some(existing_key));
        assert_eq!(drafts[0].name, "irst");
        assert!(editor.history.is_empty());
    }

    #[test]
    fn history_is_grouped_by_outcome_day_done_before_canceled() {
        let now = Utc::now();
        let mut done = item("Done");
        done.completed_at = Some(now);
        let mut canceled = item("Canceled");
        canceled.canceled_at = Some(now);
        let editor = HopperEditor::new(vec![canceled, done]);
        let date = now.with_timezone(&Local).date_naive();
        let items = editor.history_indices_for_date(date);
        assert_eq!(items.len(), 2);
        assert_eq!(editor.history[items[0]].outcome, Outcome::Done);
        assert_eq!(editor.history[items[1]].outcome, Outcome::Canceled);
    }

    #[test]
    fn a_history_item_can_be_reopened_without_closing_the_modal() {
        let now = Utc::now();
        let mut done = item("Done");
        let workspace_key = done.key.clone();
        done.completed_at = Some(now);
        let mut editor = HopperEditor::new(vec![done]);
        editor.tab = HopperTab::History;
        editor.toggle_history_day();
        editor.move_history_day(1);
        assert_eq!(
            editor.on(&key(Key::Char('r'))),
            Some(Msg::HopperCompletionRequested {
                workspace_key,
                completed: false,
            })
        );
        assert!(editor.history.is_empty());
        assert_eq!(
            editor.rows.iter().filter(|row| row.key.is_some()).count(),
            1
        );
    }

    #[test]
    fn cursor_window_keeps_the_edit_point_visible_for_long_lines() {
        let value = "a very long hopper command that does not fit";
        let (before, after) = cursor_window(value, value.len(), 13);
        assert!(before.starts_with('…'));
        assert!(after.is_empty());
        assert!(before.ends_with("does not fit"));
    }

    #[test]
    fn command_hints_wrap_without_hiding_the_delete_or_save_chords() {
        let mut editor = HopperEditor::new(vec![item("First")]);
        let rendered = render(&mut editor, 60, 24);
        // The help leads with the terminal-portable Ctrl+letter chords,
        // not the modifier+Backspace aliases that emulators may swallow.
        assert!(rendered.contains("Ctrl-X"), "{rendered}");
        assert!(rendered.contains("cancel"), "{rendered}");
        assert!(rendered.contains("Ctrl-K"), "{rendered}");
        assert!(rendered.contains("delete line"), "{rendered}");
        assert!(rendered.contains("Ctrl-S"), "{rendered}");
    }

    #[test]
    fn expanded_history_renders_counts_done_first_and_timestamps() {
        let now = Utc::now();
        let mut done = item("Finished work");
        done.completed_at = Some(now);
        let mut canceled = item("Skipped work");
        canceled.canceled_at = Some(now);
        let mut editor = HopperEditor::new(vec![canceled, done]);
        editor.tab = HopperTab::History;
        editor.toggle_history_day();
        let rendered = render(&mut editor, 90, 28);
        assert!(rendered.contains("1 done · 1 canceled"), "{rendered}");
        let done_at = rendered.find("✓ done").expect("done row");
        let canceled_at = rendered.find("× canceled").expect("canceled row");
        assert!(done_at < canceled_at, "{rendered}");
        assert!(rendered.contains("made"), "{rendered}");
    }
}
