//! Structured line editor and dated history for the personal Hopper.
//!
//! Unlike a plain textarea, every existing line retains its WorkspaceKey
//! while it is renamed or reordered. Lifecycle actions update one row in
//! place, while destructive deletion remains a separate explicit chord.

use crate::realm::{Msg, UserEvent};
use chrono::{DateTime, Local, NaiveDate, Utc};
use lazybox_core::{TodoItem, TodoLink, WorkspaceKey};
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
    /// The TODO's checklist, as the daemon last saved it.
    pub(crate) items: Vec<TodoItem>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Row {
    key: Option<WorkspaceKey>,
    name: String,
    created_at: Option<DateTime<Utc>>,
    items: Vec<TodoItem>,
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
    /// Kept so a reopened TODO comes back with its checklist.
    items: Vec<TodoItem>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HopperTab {
    Active,
    History,
}

/// Interaction mode for the Active tab, mirroring a modal editor.
///
/// Capture types text into the current row; Navigate turns bare letters
/// into lifecycle actions (`d`/`c`/`x`), so the Hopper speaks the same
/// bare-letter idiom as the rest of lazybox instead of leaning on `Ctrl`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    Capture,
    Navigate,
}

/// The checklist of one saved TODO, open inside the Active tab.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Checklist {
    /// Index of the TODO in `rows`.
    row: usize,
    /// Selected item, in display order.
    cursor: usize,
    /// Text being typed for a new item or a link, if any.
    input: Option<ChecklistInput>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ChecklistInput {
    kind: InputKind,
    text: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum InputKind {
    NewItem,
    Link,
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
    mode: Mode,
    row: usize,
    cursor: usize,
    history_cursor: usize,
    expanded_days: BTreeSet<NaiveDate>,
    error: Option<String>,
    checklist: Option<Checklist>,
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
                    items: item.items,
                });
            } else {
                rows.push(Row {
                    key: Some(item.key),
                    name: item.name,
                    created_at: Some(item.created_at),
                    items: item.items,
                });
            }
        }
        rows.push(Self::blank_row());
        let row = rows.len() - 1;
        Self {
            rows,
            history,
            tab: HopperTab::Active,
            mode: Mode::Capture,
            row,
            cursor: 0,
            history_cursor: 0,
            expanded_days: BTreeSet::new(),
            error: None,
            checklist: None,
        }
    }

    fn blank_row() -> Row {
        Row {
            key: None,
            name: String::new(),
            created_at: None,
            items: Vec::new(),
        }
    }

    fn current(&self) -> &Row {
        &self.rows[self.row]
    }

    fn current_mut(&mut self) -> &mut Row {
        &mut self.rows[self.row]
    }

    /// A saved row (it owns a workspace key) lands in Navigate mode; the
    /// trailing blank row and any still-unsaved row stay in Capture so
    /// typing keeps flowing. Called whenever the cursor changes rows.
    fn sync_mode(&mut self) {
        self.mode = if self.current().key.is_some() {
            Mode::Navigate
        } else {
            Mode::Capture
        };
    }

    fn enter_capture(&mut self) {
        self.mode = Mode::Capture;
        self.cursor = self.current().name.len();
        self.error = None;
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
        self.sync_mode();
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
                items: Vec::new(),
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
            self.error = Some("Press Esc to navigate, then c to cancel or x to delete".into());
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
        self.sync_mode();
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
            items: row.items,
        });
        self.error = None;
        self.sync_mode();
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

    /// Open the checklist of the selected TODO. Only a saved TODO has one.
    fn open_checklist(&mut self) {
        if self.current().key.is_none() {
            self.error = Some("Save this item before adding to its checklist".into());
            return;
        }
        self.checklist = Some(Checklist {
            row: self.row,
            cursor: 0,
            input: None,
        });
        self.error = None;
    }

    /// Save the open checklist: the whole list, as the daemon stores it.
    fn checklist_saved(&self, row: usize) -> Option<Msg> {
        let todo = self.rows.get(row)?;
        Some(Msg::TodoItemsChanged {
            workspace_key: todo.key.clone()?,
            items: todo.items.clone(),
        })
    }

    fn on_checklist_key(&mut self, code: Key, shift: bool) -> Option<Msg> {
        let checklist = self.checklist.clone()?;
        let row = checklist.row;
        let items = &mut self.rows[row].items;
        let cursor = checklist.cursor.min(items.len().saturating_sub(1));
        if let Some(mut input) = checklist.input {
            match code {
                Key::Esc => self.set_checklist(row, cursor, None),
                Key::Enter => {
                    let text = input.text.trim().to_string();
                    let changed = match input.kind {
                        InputKind::NewItem if !text.is_empty() => {
                            let at = insert_item_after(items, cursor, text);
                            self.set_checklist(row, at, None);
                            true
                        }
                        InputKind::NewItem => {
                            self.set_checklist(row, cursor, None);
                            false
                        }
                        InputKind::Link => {
                            if let Some(item) = items.get_mut(cursor) {
                                item.link = parse_link(&text);
                            }
                            self.set_checklist(row, cursor, None);
                            true
                        }
                    };
                    return changed.then(|| self.checklist_saved(row)).flatten();
                }
                Key::Backspace => {
                    input.text.pop();
                    self.set_checklist(row, cursor, Some(input));
                }
                Key::Char(ch) => {
                    input.text.push(ch);
                    self.set_checklist(row, cursor, Some(input));
                }
                _ => {}
            }
            return None;
        }
        match code {
            Key::Esc | Key::Char('h') | Key::Left => {
                self.checklist = None;
                None
            }
            Key::Char('j') | Key::Down => {
                let last = items.len().saturating_sub(1);
                self.set_checklist(row, (cursor + 1).min(last), None);
                None
            }
            Key::Char('k') | Key::Up => {
                self.set_checklist(row, cursor.saturating_sub(1), None);
                None
            }
            Key::Char('a') | Key::Char('o') => {
                self.set_checklist(
                    row,
                    cursor,
                    Some(ChecklistInput {
                        kind: InputKind::NewItem,
                        text: String::new(),
                    }),
                );
                None
            }
            Key::Char('L') if !items.is_empty() => {
                let text = match &items[cursor].link {
                    Some(TodoLink::Task(id)) => id.key.clone(),
                    Some(TodoLink::Url(url)) => url.clone(),
                    Some(TodoLink::Workspace(key)) => key.as_str().to_string(),
                    None => String::new(),
                };
                self.set_checklist(
                    row,
                    cursor,
                    Some(ChecklistInput {
                        kind: InputKind::Link,
                        text,
                    }),
                );
                None
            }
            Key::Enter => items
                .get(cursor)
                .and_then(|item| item.link.clone())
                .map(Msg::TodoLinkOpened),
            Key::Char(' ') if !items.is_empty() => {
                let item = &mut items[cursor];
                item.done_at = if item.is_done() {
                    None
                } else {
                    Some(Utc::now())
                };
                item.canceled_at = None;
                item.auto_checked = false;
                self.checklist_saved(row)
            }
            Key::Char('c') if !items.is_empty() => {
                let item = &mut items[cursor];
                item.canceled_at = if item.is_canceled() {
                    None
                } else {
                    Some(Utc::now())
                };
                item.done_at = None;
                self.checklist_saved(row)
            }
            Key::Tab if !shift && !items.is_empty() => indent(items, cursor)
                .then(|| self.checklist_saved(row))
                .flatten(),
            Key::BackTab | Key::Tab if !items.is_empty() => outdent(items, cursor)
                .then(|| self.checklist_saved(row))
                .flatten(),
            Key::Char('x') if !items.is_empty() => {
                remove_subtree(items, cursor);
                let last = items.len().saturating_sub(1);
                self.set_checklist(row, cursor.min(last), None);
                self.checklist_saved(row)
            }
            _ => None,
        }
    }

    fn set_checklist(&mut self, row: usize, cursor: usize, input: Option<ChecklistInput>) {
        self.checklist = Some(Checklist { row, cursor, input });
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
                items: item.items,
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
                if selected && self.mode == Mode::Capture {
                    let (before, after) = cursor_window(&row.name, self.cursor, text_width);
                    Line::from(vec![
                        Span::styled(pointer, style),
                        Span::styled("[ ] ", style.fg(theme.text_dim)),
                        Span::styled(before, style),
                        Span::styled("▌", style.fg(theme.accent)),
                        Span::styled(after, style),
                    ])
                } else {
                    let (done, total) = items_progress(&row.items);
                    let progress = progress_label(done, total);
                    let room = text_width.saturating_sub(progress.chars().count() + 2);
                    let name = crate::util::truncate_ellipsis(&row.name, room);
                    let mut spans = vec![Span::styled(format!("{pointer}[ ] {name}"), style)];
                    if total > 0 {
                        let tint = if done == total {
                            theme.success
                        } else {
                            theme.accent
                        };
                        spans.push(Span::styled(format!("  {progress}"), style.fg(tint)));
                    }
                    Line::from(spans)
                }
            })
            .collect::<Vec<_>>();
        frame.render_widget(Paragraph::new(lines), area);
    }

    fn render_checklist(
        &self,
        checklist: &Checklist,
        frame: &mut Frame,
        area: Rect,
        theme: &crate::theme::Theme,
    ) {
        let Some(todo) = self.rows.get(checklist.row) else {
            return;
        };
        let (done, total) = items_progress(&todo.items);
        let mut lines = vec![Line::from(vec![
            Span::styled(
                todo.name.clone(),
                Style::default().fg(theme.text_strong).bold(),
            ),
            Span::styled(
                format!("  {}", progress_label(done, total)),
                Style::default().fg(theme.accent),
            ),
        ])];
        if todo.items.is_empty() && checklist.input.is_none() {
            lines.push(Line::from(Span::styled(
                "  No items yet — press a to add one.",
                Style::default().fg(theme.text_dim),
            )));
        }
        for (index, item) in todo.items.iter().enumerate() {
            let selected = index == checklist.cursor && checklist.input.is_none();
            let style = if selected {
                theme.row_focused()
            } else if item.is_canceled() || item.is_done() {
                Style::default().fg(theme.text_dim)
            } else {
                Style::default().fg(theme.text_strong)
            };
            let indent = "  ".repeat(depth_of(&todo.items, index));
            let boxed = match (item.is_done(), item.is_canceled()) {
                (true, _) => "[x]",
                (_, true) => "[-]",
                _ => "[ ]",
            };
            let mut spans = vec![Span::styled(
                format!(
                    "{}{indent}{boxed} {}",
                    if selected { "> " } else { "  " },
                    item.text
                ),
                if item.is_canceled() {
                    style.add_modifier(Modifier::CROSSED_OUT)
                } else {
                    style
                },
            )];
            if let Some(link) = &item.link {
                let target = match link {
                    TodoLink::Task(id) => id.key.clone(),
                    TodoLink::Workspace(key) => key.as_str().to_string(),
                    TodoLink::Url(url) => url.clone(),
                };
                spans.push(Span::styled(
                    format!("  → {target}"),
                    style.fg(theme.accent).add_modifier(Modifier::UNDERLINED),
                ));
            }
            if item.auto_checked {
                spans.push(Span::styled("  auto", style.fg(theme.text_dim)));
            }
            lines.push(Line::from(spans));
        }
        if let Some(input) = &checklist.input {
            let label = match input.kind {
                InputKind::NewItem => "new item: ",
                InputKind::Link => "link (owner/repo#N or URL, empty clears): ",
            };
            lines.push(Line::from(vec![
                Span::styled(format!("  {label}"), Style::default().fg(theme.text_dim)),
                Span::styled(input.text.clone(), Style::default().fg(theme.text_strong)),
                Span::styled("▌", Style::default().fg(theme.accent)),
            ]));
        }
        let skip = lines.len().saturating_sub(area.height as usize);
        frame.render_widget(
            Paragraph::new(lines.into_iter().skip(skip).collect::<Vec<_>>()),
            area,
        );
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

/// Nesting depth of `items[index]`: how many parents it has.
fn depth_of(items: &[TodoItem], index: usize) -> usize {
    let mut depth = 0;
    let mut parent = items[index].parent.as_deref();
    while let Some(id) = parent {
        depth += 1;
        if depth > items.len() {
            break;
        }
        parent = items
            .iter()
            .find(|i| i.id == id)
            .and_then(|i| i.parent.as_deref());
    }
    depth
}

/// One past the last descendant of `items[index]` — where its subtree ends
/// in display order.
fn subtree_end(items: &[TodoItem], index: usize) -> usize {
    let depth = depth_of(items, index);
    let mut end = index + 1;
    while end < items.len() && depth_of(items, end) > depth {
        end += 1;
    }
    end
}

/// Add an item after `items[cursor]` and its subtree, as its sibling (the
/// first item of an empty list sits at the top level). Returns the new
/// item's index. The id is minted here, so it is final from the first save.
fn insert_item_after(items: &mut Vec<TodoItem>, cursor: usize, text: String) -> usize {
    let (at, parent) = if items.is_empty() {
        (0, None)
    } else {
        (subtree_end(items, cursor), items[cursor].parent.clone())
    };
    items.insert(
        at,
        TodoItem {
            id: TodoItem::new_id(),
            parent,
            text,
            done_at: None,
            canceled_at: None,
            link: None,
            auto_checked: false,
        },
    );
    at
}

/// Nest `items[cursor]` under its previous sibling. False when there is
/// none (the first child of its parent cannot go deeper).
fn indent(items: &mut [TodoItem], cursor: usize) -> bool {
    let depth = depth_of(items, cursor);
    let sibling = (0..cursor)
        .rev()
        .take_while(|&i| depth_of(items, i) >= depth)
        .find(|&i| depth_of(items, i) == depth);
    let Some(sibling) = sibling else {
        return false;
    };
    items[cursor].parent = Some(items[sibling].id.clone());
    true
}

/// Lift `items[cursor]` to its parent's level. False at the top level.
fn outdent(items: &mut [TodoItem], cursor: usize) -> bool {
    let Some(parent) = items[cursor].parent.clone() else {
        return false;
    };
    let grandparent = items
        .iter()
        .find(|i| i.id == parent)
        .and_then(|i| i.parent.clone());
    items[cursor].parent = grandparent;
    true
}

/// Delete `items[cursor]` and everything nested under it.
fn remove_subtree(items: &mut Vec<TodoItem>, cursor: usize) {
    let end = subtree_end(items, cursor);
    items.drain(cursor..end);
}

/// What a typed link names: an issue / PR reference, or a URL. Empty text
/// clears the link.
fn parse_link(text: &str) -> Option<TodoLink> {
    let text = text.trim();
    if text.is_empty() {
        return None;
    }
    if let Some(task) = lazybox_core::task_ref::parse_task_ref(text, None) {
        return Some(TodoLink::Task(task));
    }
    Some(TodoLink::Url(text.to_string()))
}

/// GitHub-task-list style progress: `▰▰▱ 2/3`, at most eight cells.
pub(crate) fn progress_label(done: usize, total: usize) -> String {
    if total == 0 {
        return String::new();
    }
    let cells = total.min(8);
    let filled = (done * cells + total / 2) / total;
    format!(
        "{}{} {done}/{total}",
        "▰".repeat(filled),
        "▱".repeat(cells - filled)
    )
}

/// `(done, total)` over a checklist, canceled items left out.
fn items_progress(items: &[TodoItem]) -> (usize, usize) {
    items
        .iter()
        .filter(|i| !i.is_canceled())
        .fold((0, 0), |(d, t), i| (d + usize::from(i.is_done()), t + 1))
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
            .title(Span::styled(" TODO ", theme.modal_title()))
            .border_style(theme.modal_border());
        let inner = block.inner(modal);
        frame.render_widget(block, modal);

        let active_count = self
            .rows
            .iter()
            .filter(|row| row.key.is_some() || !row.name.trim().is_empty())
            .count();
        let mut tabs = Line::from(vec![
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
        if self.tab == HopperTab::Active {
            let (label, tint) = match self.mode {
                Mode::Capture => ("  ● CAPTURE", theme.accent),
                Mode::Navigate => ("  ● NAVIGATE", theme.success),
            };
            tabs.spans
                .push(Span::styled(label, Style::default().fg(tint).bold()));
        }
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
        match (self.tab, &self.checklist) {
            (HopperTab::Active, Some(checklist)) => {
                self.render_checklist(checklist, frame, body, theme)
            }
            (HopperTab::Active, None) => self.render_active(frame, body, theme),
            (HopperTab::History, _) => self.render_history(frame, body, theme),
        }

        let checklist_help = self.checklist.as_ref().map(|_| {
            vec![
                Line::from(vec![
                    Span::styled("a", Style::default().fg(theme.success).bold()),
                    Span::raw(" add  "),
                    Span::styled("Space", Style::default().fg(theme.success).bold()),
                    Span::raw(" done  "),
                    Span::styled("c", Style::default().fg(theme.text_dim).bold()),
                    Span::raw(" cancel  "),
                    Span::styled("Tab/S-Tab", Style::default().fg(theme.text_dim).bold()),
                    Span::raw(" nest  "),
                    Span::styled("x", Style::default().fg(theme.error).bold()),
                    Span::raw(" delete"),
                ]),
                Line::from(vec![
                    Span::styled("L", Style::default().fg(theme.success).bold()),
                    Span::raw(" link  "),
                    Span::styled("Enter", Style::default().fg(theme.success).bold()),
                    Span::raw(" open link  "),
                    Span::styled("h/Esc", Style::default().fg(theme.text_dim).bold()),
                    Span::raw(" back"),
                ]),
                Line::from(Span::styled(
                    "An item linked to a PR or issue checks itself off when it merges or closes.",
                    Style::default().fg(theme.text_dim),
                )),
            ]
        });
        let mut help = if let (HopperTab::Active, Some(lines)) = (self.tab, checklist_help) {
            lines
        } else {
            match self.tab {
                HopperTab::Active => match self.mode {
                    Mode::Navigate => vec![
                        Line::from(vec![
                            Span::styled("d", Style::default().fg(theme.success).bold()),
                            Span::raw(" done  "),
                            Span::styled("c", Style::default().fg(theme.text_dim).bold()),
                            Span::raw(" cancel  "),
                            Span::styled("x", Style::default().fg(theme.error).bold()),
                            Span::raw(" delete  "),
                            Span::styled("j/k", Style::default().fg(theme.text_dim).bold()),
                            Span::raw(" move  "),
                            Span::styled("i", Style::default().fg(theme.success).bold()),
                            Span::raw(" edit  "),
                            Span::styled("l", Style::default().fg(theme.success).bold()),
                            Span::raw(" checklist"),
                        ]),
                        Line::from(vec![
                            Span::styled("Tab", Style::default().fg(theme.success).bold()),
                            Span::raw(" history  "),
                            Span::styled("Esc", Style::default().fg(theme.error).bold()),
                            Span::raw(" close"),
                        ]),
                        Line::from(Span::styled(
                            "Bare letters act on this saved item. Move to the blank row to capture a new one.",
                            Style::default().fg(theme.text_dim),
                        )),
                    ],
                    Mode::Capture => vec![
                        Line::from(vec![
                            Span::styled("Enter", Style::default().fg(theme.success).bold()),
                            Span::raw(" next item / save  "),
                            Span::styled("↑↓", Style::default().fg(theme.text_dim).bold()),
                            Span::raw(" move  "),
                            Span::styled("Esc", Style::default().fg(theme.error).bold()),
                            Span::raw(" close"),
                        ]),
                        Line::from(Span::styled(
                            "Type to edit this item. Enter on the empty row saves and closes; move onto a saved item for d/c/x.",
                            Style::default().fg(theme.text_dim),
                        )),
                        Line::from(Span::styled(
                            "One line is one persistent workspace. Paste newline-separated items to capture in bulk.",
                            Style::default().fg(theme.text_dim),
                        )),
                    ],
                },
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
            }
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
            if let Some(Checklist {
                input: Some(input), ..
            }) = self.checklist.as_mut()
            {
                input.text.extend(text.chars().filter(|c| !c.is_control()));
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
        if ctrl && matches!(key.code, Key::Char('c')) {
            return Some(Msg::ModalDismissed);
        }
        // An open checklist owns every key, Tab and Esc included (nest and
        // back), so it is routed before the modal-level bindings.
        if self.tab == HopperTab::Active && self.checklist.is_some() {
            let shift = key.modifiers.contains(KeyModifiers::SHIFT);
            return self.on_checklist_key(key.code, shift);
        }
        if matches!(key.code, Key::Esc) {
            // Esc leaves an in-place rename (Capture on a saved row) back
            // to Navigate — the vim insert→normal transition — instead of
            // closing the whole modal. Everywhere else it dismisses.
            if self.tab == HopperTab::Active
                && self.mode == Mode::Capture
                && self.current().key.is_some()
            {
                self.sync_mode();
                self.error = None;
                return None;
            }
            return Some(Msg::ModalDismissed);
        }
        if matches!(key.code, Key::Tab | Key::BackTab) {
            self.tab = match self.tab {
                HopperTab::Active => HopperTab::History,
                HopperTab::History => HopperTab::Active,
            };
            if self.tab == HopperTab::Active {
                self.sync_mode();
            }
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
        // Active tab is a modal editor. Navigate mode turns bare letters
        // into lifecycle actions on a saved row; Capture mode edits text.
        // No Ctrl chord is required for either — the whole point of #1423.
        if self.mode == Mode::Navigate {
            match key.code {
                Key::Char('j') | Key::Down => self.move_vertical(1),
                Key::Char('k') | Key::Up => self.move_vertical(-1),
                Key::Char('d') => return self.move_current_to_history(Outcome::Done),
                Key::Char('c') => return self.move_current_to_history(Outcome::Canceled),
                Key::Char('x') => return self.delete_current_line(),
                Key::Char('i') | Key::Enter => self.enter_capture(),
                Key::Char('l') | Key::Right => self.open_checklist(),
                _ => return None,
            }
            return None;
        }
        match key.code {
            Key::Delete => self.delete_forward(),
            // Enter commits the row: a new item on the empty trailing row
            // saves the buffer and closes; otherwise it starts the next
            // item. This is the Ctrl-free replacement for the old Ctrl-S.
            Key::Enter => {
                if self.current().name.trim().is_empty() {
                    return self.drafts().map(Msg::HopperSubmitted);
                }
                self.insert_row_break();
            }
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
            items: Vec::new(),
        }
    }

    fn key(code: Key) -> Event<UserEvent> {
        Event::Keyboard(KeyEvent {
            code,
            modifiers: KeyModifiers::NONE,
        })
    }

    /// A key carrying a modifier — the mode split dropped every Ctrl
    /// binding, so this exists to prove the dropped ones stay inert.
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

    /// Open the checklist of the first (saved) row.
    fn checklist_editor() -> HopperEditor {
        let mut editor = HopperEditor::new(vec![item("Ship 0.1.18")]);
        editor.on(&key(Key::Up));
        assert_eq!(editor.mode, Mode::Navigate);
        editor.on(&key(Key::Char('l')));
        assert!(editor.checklist.is_some(), "l opens the checklist");
        editor
    }

    fn type_text(editor: &mut HopperEditor, text: &str) {
        for ch in text.chars() {
            editor.on(&key(Key::Char(ch)));
        }
    }

    fn saved(msg: Option<Msg>) -> Vec<TodoItem> {
        match msg {
            Some(Msg::TodoItemsChanged { items, .. }) => items,
            other => panic!("expected a checklist save, got {other:?}"),
        }
    }

    /// Adding, nesting and checking items each save the whole list, with a
    /// permanent id minted on the first save.
    #[test]
    fn checklist_items_are_added_nested_and_checked_off() {
        let mut editor = checklist_editor();
        editor.on(&key(Key::Char('a')));
        type_text(&mut editor, "cut the release");
        let items = saved(editor.on(&key(Key::Enter)));
        assert_eq!(items.len(), 1);
        let id = items[0].id.clone();
        assert!(!id.is_empty(), "the id is minted on the first save");

        editor.on(&key(Key::Char('a')));
        type_text(&mut editor, "run the preflight");
        assert_eq!(saved(editor.on(&key(Key::Enter))).len(), 2);
        let items = saved(editor.on(&key(Key::Tab)));
        assert_eq!(items[1].parent.as_deref(), Some(id.as_str()), "Tab nests");
        assert_eq!(items[0].id, id, "the id did not change");

        let items = saved(editor.on(&key(Key::Char(' '))));
        assert!(items[1].is_done(), "Space checks off");
        assert_eq!(items_progress(&items), (1, 2));

        let items = saved(editor.on(&key(Key::BackTab)));
        assert_eq!(items[1].parent, None, "Shift-Tab lifts it back out");
    }

    /// A link typed as `owner/repo#N` becomes a task link, and Enter on
    /// the item asks to open it.
    #[test]
    fn a_linked_item_opens_its_link() {
        let mut editor = checklist_editor();
        editor.on(&key(Key::Char('a')));
        type_text(&mut editor, "merge the PR");
        editor.on(&key(Key::Enter));
        editor.on(&key(Key::Char('L')));
        type_text(&mut editor, "o/r#1890");
        let items = saved(editor.on(&key(Key::Enter)));
        let Some(TodoLink::Task(task)) = &items[0].link else {
            panic!("a task link: {:?}", items[0].link);
        };
        assert_eq!(task.key, "o/r#1890");
        assert!(matches!(
            editor.on(&key(Key::Enter)),
            Some(Msg::TodoLinkOpened(TodoLink::Task(_)))
        ));
    }

    /// `x` removes an item with everything nested under it; Esc goes back
    /// to the TODO list rather than closing the modal.
    #[test]
    fn x_removes_a_subtree_and_esc_leaves_the_checklist() {
        let mut editor = checklist_editor();
        for text in ["parent", "child"] {
            editor.on(&key(Key::Char('a')));
            type_text(&mut editor, text);
            editor.on(&key(Key::Enter));
        }
        editor.on(&key(Key::Tab));
        editor.on(&key(Key::Char('k')));
        assert!(saved(editor.on(&key(Key::Char('x')))).is_empty());
        assert_eq!(editor.on(&key(Key::Esc)), None);
        assert!(editor.checklist.is_none(), "Esc left the checklist");
    }

    /// Each TODO line carries its progress.
    #[test]
    fn a_todo_line_shows_its_progress() {
        let mut todo = item("Ship 0.1.18");
        let mut done = TodoItem {
            id: "a".into(),
            parent: None,
            text: "a".into(),
            done_at: None,
            canceled_at: None,
            link: None,
            auto_checked: false,
        };
        let open = done.clone();
        done.done_at = Some(Utc::now());
        todo.items = vec![
            done,
            TodoItem {
                id: "b".into(),
                ..open
            },
        ];
        let mut editor = HopperEditor::new(vec![todo]);
        let screen = render(&mut editor, 90, 20);
        assert!(screen.contains("Ship 0.1.18  ▰▱ 1/2"), "{screen}");
        assert_eq!(progress_label(0, 0), "");
        assert_eq!(progress_label(3, 3), "▰▰▰ 3/3");
    }

    #[test]
    fn enter_creates_a_new_identity_without_losing_the_existing_one() {
        let existing = item("First");
        let existing_key = existing.key.clone();
        let mut editor = HopperEditor::new(vec![existing]);
        editor.on(&key(Key::Up)); // saved row → navigate mode
        editor.on(&key(Key::Char('i'))); // edit in place → capture mode
        editor.on(&key(Key::Enter)); // commit the row, start the next
        editor.on(&key(Key::Char('N')));
        let drafts = editor.drafts().expect("valid drafts");
        assert_eq!(drafts[0].workspace_key, Some(existing_key));
        assert_eq!(drafts[1].workspace_key, None);
        assert_eq!(drafts[1].name, "N");
    }

    #[test]
    fn a_saved_row_enters_navigate_and_the_blank_row_is_capture() {
        let mut editor = HopperEditor::new(vec![item("First")]);
        assert_eq!(editor.mode, Mode::Capture, "opens on the blank capture row");
        editor.on(&key(Key::Up));
        assert_eq!(editor.mode, Mode::Navigate, "a saved row navigates");
        editor.on(&key(Key::Down));
        assert_eq!(editor.mode, Mode::Capture, "the blank row captures");
        editor.on(&key(Key::Char('a')));
        let drafts = editor.drafts().expect("valid drafts");
        assert_eq!(drafts.last().expect("captured row").name, "a");
    }

    #[test]
    fn i_edits_a_saved_row_and_esc_returns_to_navigate() {
        let mut editor = HopperEditor::new(vec![item("First")]);
        editor.on(&key(Key::Up)); // navigate
        editor.on(&key(Key::Char('i'))); // capture, cursor at end
        editor.on(&key(Key::Char('!')));
        assert_eq!(editor.mode, Mode::Capture);
        assert_eq!(editor.rows[editor.row].name, "First!");
        assert_eq!(editor.on(&key(Key::Esc)), None, "Esc leaves the rename");
        assert_eq!(editor.mode, Mode::Navigate);
    }

    #[test]
    fn enter_on_the_empty_row_saves_and_closes() {
        let existing = item("First");
        let existing_key = existing.key.clone();
        let mut editor = HopperEditor::new(vec![existing]);
        // Cursor opens on the empty trailing capture row.
        let submitted = editor.on(&key(Key::Enter)).expect("save");
        let Msg::HopperSubmitted(entries) = submitted else {
            panic!("expected HopperSubmitted, got {submitted:?}");
        };
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].workspace_key, Some(existing_key));
    }

    #[test]
    fn done_and_cancel_are_in_place_and_accumulate_in_history() {
        let first = item("First");
        let second = item("Second");
        let first_key = first.key.clone();
        let second_key = second.key.clone();
        let mut editor = HopperEditor::new(vec![first, second]);
        editor.on(&key(Key::Up)); // Second → navigate
        editor.on(&key(Key::Up)); // First → navigate
        assert_eq!(
            editor.on(&key(Key::Char('d'))),
            Some(Msg::HopperCompletionRequested {
                workspace_key: first_key,
                completed: true,
            })
        );
        // The cursor lands on Second, still a saved row → still navigate.
        assert_eq!(editor.mode, Mode::Navigate);
        assert_eq!(
            editor.on(&key(Key::Char('c'))),
            Some(Msg::HopperCancellationRequested {
                workspace_key: second_key,
                canceled: true,
            })
        );
        assert_eq!(editor.history.len(), 2);
        assert_eq!(editor.rows.len(), 1, "only the capture row remains");
    }

    #[test]
    fn c_cancels_into_history_in_navigate_mode() {
        let existing = item("First");
        let existing_key = existing.key.clone();
        let mut editor = HopperEditor::new(vec![existing]);
        editor.on(&key(Key::Up));
        assert_eq!(
            editor.on(&key(Key::Char('c'))),
            Some(Msg::HopperCancellationRequested {
                workspace_key: existing_key,
                canceled: true,
            })
        );
        assert_eq!(editor.history.len(), 1);
        assert_eq!(editor.history[0].outcome, Outcome::Canceled);
    }

    #[test]
    fn x_deletes_the_whole_line_in_navigate_mode() {
        let existing = item("First");
        let existing_key = existing.key.clone();
        let mut editor = HopperEditor::new(vec![existing]);
        editor.on(&key(Key::Up));
        assert_eq!(
            editor.on(&key(Key::Char('x'))),
            Some(Msg::HopperDeleteRequested(existing_key))
        );
        assert!(editor.history.is_empty());
        assert_eq!(editor.rows.len(), 1);
    }

    /// #1422's guard, carried across the #1423 mode split. Ctrl-K is
    /// readline kill-to-EOL — a harmless edit — and must never reap a
    /// workspace. The mode split removed the Ctrl bindings entirely
    /// (destruction is bare `x` in navigate mode), so this now pins that
    /// Ctrl-K is *inert* in both modes rather than merely rebound: the
    /// original bug was a keystroke people press by reflex deleting
    /// their work, and nothing else would catch it coming back.
    #[test]
    fn ctrl_k_never_reaps_a_workspace_in_either_mode() {
        let existing = item("First");
        let existing_key = existing.key.clone();
        let mut editor = HopperEditor::new(vec![existing]);

        // Navigate mode, sitting on the saved item — where `x` deletes.
        editor.on(&key(Key::Up));
        assert_eq!(
            editor.on(&modified(Key::Char('k'), KeyModifiers::CONTROL)),
            None,
            "Ctrl-K must not act in navigate mode",
        );
        assert!(editor.history.is_empty());
        assert_eq!(editor.rows.len(), 2, "the row survives");

        // Capture mode: Ctrl-K must not type a `k` either — a modified
        // key is a command, not text.
        editor.on(&key(Key::Char('i')));
        let before = editor.current().name.clone();
        assert_eq!(
            editor.on(&modified(Key::Char('k'), KeyModifiers::CONTROL)),
            None,
            "Ctrl-K must not act in capture mode",
        );
        assert_eq!(editor.current().name, before, "and must not insert text");
        assert!(editor.history.is_empty());
        let drafts = editor.drafts().expect("valid drafts");
        assert_eq!(drafts[0].workspace_key, Some(existing_key));
    }

    #[test]
    fn lifecycle_letters_are_plain_text_in_capture_mode() {
        // On the blank capture row, d/c/x type rather than acting.
        let mut editor = HopperEditor::new(vec![item("First")]);
        for ch in ['d', 'c', 'x'] {
            assert_eq!(editor.on(&key(Key::Char(ch))), None);
        }
        assert!(editor.history.is_empty());
        let drafts = editor.drafts().expect("valid drafts");
        assert_eq!(drafts.last().expect("captured row").name, "dcx");
    }

    #[test]
    fn plain_delete_remains_a_text_editing_key() {
        let existing = item("First");
        let existing_key = existing.key.clone();
        let mut editor = HopperEditor::new(vec![existing]);
        editor.on(&key(Key::Up)); // navigate
        editor.on(&key(Key::Char('i'))); // edit → capture, cursor at end
        editor.on(&key(Key::Home));
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
    fn command_hints_are_bare_letters_not_ctrl_chords() {
        // Capture mode leads with the save/next Enter idiom.
        let mut editor = HopperEditor::new(vec![item("First")]);
        let capture = render(&mut editor, 70, 24);
        assert!(capture.contains("CAPTURE"), "{capture}");
        assert!(capture.contains("save"), "{capture}");
        assert!(!capture.contains("Ctrl"), "no Ctrl in capture: {capture}");

        // Navigate mode surfaces the bare-letter lifecycle actions.
        editor.on(&key(Key::Up));
        let navigate = render(&mut editor, 70, 24);
        assert!(navigate.contains("NAVIGATE"), "{navigate}");
        assert!(navigate.contains("done"), "{navigate}");
        assert!(navigate.contains("cancel"), "{navigate}");
        assert!(navigate.contains("delete"), "{navigate}");
        assert!(
            !navigate.contains("Ctrl"),
            "no Ctrl in navigate: {navigate}"
        );
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
