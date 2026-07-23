//! `Textarea` — multi-line text input with readline-style editing.
//! tuirealm port of `tui_kit::widgets::TextareaModal`.
//!
//! Bindings:
//! - `Ctrl-Enter` / `Ctrl-S` — submit (returns `Msg::TextareaSubmitted`).
//! - `Enter` — newline.
//! - `Esc` / `Ctrl-C` — cancel (returns `Msg::ModalDismissed`).
//! - `Ctrl-A` / `Home` — line start.
//! - `Ctrl-E` / `End` — line end.
//! - `Alt-B` / `Alt-Left` — word back.
//! - `Alt-F` / `Alt-Right` — word forward.
//! - `Ctrl-K` — kill to line end.
//! - `Ctrl-U` — kill to line start.
//! - `Ctrl-W` / `Ctrl-Backspace` — kill word back.

use crate::realm::Msg;
use crate::realm::UserEvent;
use tuirealm::command::{Cmd, CmdResult};
use tuirealm::component::{AppComponent, Component};
use tuirealm::event::{Event, Key, KeyModifiers};
use tuirealm::props::{AttrValue, Attribute, QueryResult};
use tuirealm::ratatui::Frame;
use tuirealm::ratatui::layout::Rect;
use tuirealm::ratatui::prelude::*;
use tuirealm::ratatui::widgets::{Block, BorderType, Borders, Clear, Paragraph, Wrap};
use tuirealm::state::State;

/// Multi-line textarea.
pub struct Textarea {
    title: String,
    /// Optional dimmed/italic header rendered above the buffer.
    header: Option<String>,
    /// Edited buffer.
    buffer: String,
    /// Cursor byte position.
    cursor: usize,
    /// Last error (e.g. empty submit) — shown in the help row.
    error: Option<String>,
    /// Allow submitting an empty buffer. Off by default (a reply or a
    /// broadcast must carry text); the notes editor turns it on so an
    /// emptied buffer submits as a "clear the scratchpad" (issue #458).
    allow_empty: bool,
}

impl Textarea {
    /// Construct with the given title.
    pub fn new(title: impl Into<String>) -> Self {
        Self {
            title: title.into(),
            header: None,
            buffer: String::new(),
            cursor: 0,
            error: None,
            allow_empty: false,
        }
    }

    /// Pre-fill the buffer; cursor lands at end.
    pub fn with_body(mut self, text: impl Into<String>) -> Self {
        self.buffer = text.into();
        self.cursor = self.buffer.len();
        self
    }

    /// Permit submitting an empty buffer (default: rejected). Used by
    /// the notes editor, where an emptied buffer means "clear the note".
    pub fn allow_empty(mut self) -> Self {
        self.allow_empty = true;
        self
    }

    /// Set the dimmed header line.
    pub fn with_header(mut self, header: impl Into<String>) -> Self {
        self.header = Some(header.into());
        self
    }

    fn insert_char(&mut self, c: char) {
        self.buffer.insert(self.cursor, c);
        self.cursor += c.len_utf8();
        self.error = None;
    }

    fn insert_newline(&mut self) {
        self.insert_char('\n');
    }

    fn delete_back(&mut self) {
        if self.cursor == 0 {
            return;
        }
        let before = &self.buffer[..self.cursor];
        let last = before.chars().next_back().unwrap();
        let new_pos = self.cursor - last.len_utf8();
        self.buffer.replace_range(new_pos..self.cursor, "");
        self.cursor = new_pos;
        self.error = None;
    }

    fn move_left(&mut self) {
        if self.cursor == 0 {
            return;
        }
        let before = &self.buffer[..self.cursor];
        let last = before.chars().next_back().unwrap();
        self.cursor -= last.len_utf8();
    }

    fn move_right(&mut self) {
        if self.cursor >= self.buffer.len() {
            return;
        }
        let after = &self.buffer[self.cursor..];
        let next = after.chars().next().unwrap();
        self.cursor += next.len_utf8();
    }

    fn move_line_start(&mut self) {
        let before = &self.buffer[..self.cursor];
        match before.rfind('\n') {
            Some(i) => self.cursor = i + 1,
            None => self.cursor = 0,
        }
    }

    fn move_line_end(&mut self) {
        let after = &self.buffer[self.cursor..];
        match after.find('\n') {
            Some(i) => self.cursor += i,
            None => self.cursor = self.buffer.len(),
        }
    }

    fn move_word_back(&mut self) {
        let before = &self.buffer[..self.cursor];
        let mut chars: Vec<char> = before.chars().collect();
        while chars.last().is_some_and(|c| c.is_whitespace()) {
            chars.pop();
        }
        while chars.last().is_some_and(|c| !c.is_whitespace()) {
            chars.pop();
        }
        self.cursor = chars.iter().map(|c| c.len_utf8()).sum::<usize>();
    }

    fn move_word_forward(&mut self) {
        let after = &self.buffer[self.cursor..];
        let mut byte_advance = 0usize;
        let mut chars = after.chars().peekable();
        while chars.peek().is_some_and(|c| !c.is_whitespace()) {
            byte_advance += chars.next().unwrap().len_utf8();
        }
        while chars.peek().is_some_and(|c| c.is_whitespace()) {
            byte_advance += chars.next().unwrap().len_utf8();
        }
        self.cursor = (self.cursor + byte_advance).min(self.buffer.len());
    }

    fn kill_to_line_end(&mut self) {
        let after = &self.buffer[self.cursor..];
        let end = match after.find('\n') {
            Some(i) => self.cursor + i,
            None => self.buffer.len(),
        };
        self.buffer.replace_range(self.cursor..end, "");
        self.error = None;
    }

    fn kill_to_line_start(&mut self) {
        let before = &self.buffer[..self.cursor];
        let start = before.rfind('\n').map(|i| i + 1).unwrap_or(0);
        self.buffer.replace_range(start..self.cursor, "");
        self.cursor = start;
        self.error = None;
    }

    fn kill_word_back(&mut self) {
        let from = self.cursor;
        self.move_word_back();
        let to = self.cursor;
        self.buffer.replace_range(to..from, "");
        self.error = None;
    }

    fn render_lines(&self) -> (Vec<String>, usize, usize) {
        let mut lines: Vec<String> = self.buffer.split('\n').map(str::to_string).collect();
        if lines.is_empty() {
            lines.push(String::new());
        }
        let mut cursor_line = 0usize;
        let mut bytes = 0usize;
        for (i, line) in lines.iter().enumerate() {
            let end = bytes + line.len();
            if self.cursor <= end {
                cursor_line = i;
                break;
            }
            bytes = end + 1;
        }
        let line_start = {
            let mut acc = 0usize;
            for (i, line) in lines.iter().enumerate() {
                if i == cursor_line {
                    break;
                }
                acc += line.len() + 1;
            }
            acc
        };
        let cursor_col = self.buffer[line_start..self.cursor].chars().count();
        (lines, cursor_line, cursor_col)
    }
}

impl Component for Textarea {
    fn view(&mut self, frame: &mut Frame, area: Rect) {
        let theme = crate::theme::current();
        let modal_w = 80u16.min(area.width.saturating_sub(4));
        let modal_h = 22u16.min(area.height.saturating_sub(4));
        let x = area.x + area.width.saturating_sub(modal_w) / 2;
        let y = area.y + area.height.saturating_sub(modal_h) / 2;
        let modal = Rect::new(x, y, modal_w, modal_h);

        frame.render_widget(Clear, modal);
        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .title(Span::styled(
                format!(" {} ", self.title),
                theme.modal_title(),
            ))
            .border_style(theme.modal_border());
        let inner = block.inner(modal);
        frame.render_widget(block, modal);

        let header_h: u16 = if self.header.is_some() { 2 } else { 0 };
        let help_h: u16 = 2;
        let body_h = inner.height.saturating_sub(header_h + help_h);
        let header_area = Rect {
            x: inner.x,
            y: inner.y,
            width: inner.width,
            height: header_h,
        };
        let body_area = Rect {
            x: inner.x,
            y: inner.y + header_h,
            width: inner.width,
            height: body_h,
        };
        let help_area = Rect {
            x: inner.x,
            y: inner.y + header_h + body_h,
            width: inner.width,
            height: help_h,
        };

        if let Some(header) = &self.header {
            let lines = vec![
                Line::from(Span::styled(
                    header.clone(),
                    Style::default()
                        .fg(theme.text_dim)
                        .add_modifier(Modifier::ITALIC),
                )),
                Line::from(Span::styled(
                    "─".repeat(inner.width as usize),
                    theme.divider(),
                )),
            ];
            frame.render_widget(Paragraph::new(lines), header_area);
        }

        let (lines, cursor_line, cursor_col) = self.render_lines();
        let mut rendered: Vec<Line<'static>> = Vec::with_capacity(lines.len());
        for (i, line) in lines.iter().enumerate() {
            if i == cursor_line {
                let col = cursor_col.min(line.chars().count());
                let prefix: String = line.chars().take(col).collect();
                let suffix: String = line.chars().skip(col).collect();
                rendered.push(Line::from(vec![
                    Span::styled(prefix, Style::default().fg(theme.text_strong)),
                    Span::styled("▌", Style::default().fg(theme.accent)),
                    Span::styled(suffix, Style::default().fg(theme.text_strong)),
                ]));
            } else {
                rendered.push(Line::from(Span::styled(
                    line.clone(),
                    Style::default().fg(theme.text_strong),
                )));
            }
        }
        frame.render_widget(
            Paragraph::new(rendered).wrap(Wrap { trim: false }),
            body_area,
        );

        let mut help_spans = vec![
            Span::styled("Ctrl-Enter", Style::default().fg(theme.success).bold()),
            Span::raw(" send  "),
            Span::styled("Esc", Style::default().fg(theme.error).bold()),
            Span::raw(" cancel  "),
            Span::styled("Enter", Style::default().fg(theme.text_dim).bold()),
            Span::raw(" newline"),
        ];
        if let Some(err) = &self.error {
            help_spans.push(Span::raw("    "));
            help_spans.push(Span::styled(err.clone(), Style::default().fg(theme.error)));
        }
        frame.render_widget(
            Paragraph::new(vec![Line::raw(""), Line::from(help_spans)]),
            help_area,
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

impl AppComponent<Msg, UserEvent> for Textarea {
    fn on(&mut self, ev: &Event<UserEvent>) -> Option<Msg> {
        // Bracketed paste — Cmd-V / Ctrl-V into the reply textarea.
        // Host terminal wraps the clipboard content; crossterm
        // surfaces it as `Event::Paste(text)`. Insert the whole
        // string at the cursor in one shot rather than running the
        // per-key dispatch below (avoids triggering Enter handling
        // for newlines in the pasted text, which would split the
        // submit / insert-newline path).
        if let Event::Paste(text) = ev {
            for ch in text.chars() {
                if ch == '\n' {
                    self.insert_newline();
                } else if !ch.is_control() {
                    self.insert_char(ch);
                }
            }
            return None;
        }
        let Event::Keyboard(key) = ev else {
            return None;
        };
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let alt = key.modifiers.contains(KeyModifiers::ALT);
        // Cancel keys.
        if matches!(key.code, Key::Esc) || (ctrl && matches!(key.code, Key::Char('c'))) {
            return Some(Msg::ModalDismissed);
        }
        // Submit keys (Ctrl-Enter / Ctrl-S).
        if ctrl && matches!(key.code, Key::Enter | Key::Char('s')) {
            if self.buffer.trim().is_empty() && !self.allow_empty {
                self.error = Some("can't submit empty input".into());
                return None;
            }
            return Some(Msg::TextareaSubmitted(self.buffer.clone()));
        }
        match key.code {
            Key::Enter => {
                self.insert_newline();
                None
            }
            Key::Backspace if ctrl => {
                self.kill_word_back();
                None
            }
            Key::Backspace => {
                self.delete_back();
                None
            }
            Key::Left if alt => {
                self.move_word_back();
                None
            }
            Key::Left => {
                self.move_left();
                None
            }
            Key::Right if alt => {
                self.move_word_forward();
                None
            }
            Key::Right => {
                self.move_right();
                None
            }
            Key::Home => {
                self.move_line_start();
                None
            }
            Key::End => {
                self.move_line_end();
                None
            }
            Key::Char('a') if ctrl => {
                self.move_line_start();
                None
            }
            Key::Char('e') if ctrl => {
                self.move_line_end();
                None
            }
            Key::Char('k') if ctrl => {
                self.kill_to_line_end();
                None
            }
            Key::Char('u') if ctrl => {
                self.kill_to_line_start();
                None
            }
            Key::Char('w') if ctrl => {
                self.kill_word_back();
                None
            }
            Key::Char('b') if alt => {
                self.move_word_back();
                None
            }
            Key::Char('f') if alt => {
                self.move_word_forward();
                None
            }
            Key::Char(c) if !ctrl => {
                self.insert_char(c);
                None
            }
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tuirealm::event::KeyEvent;

    fn ctrl_s() -> Event<UserEvent> {
        Event::Keyboard(KeyEvent {
            code: Key::Char('s'),
            modifiers: KeyModifiers::CONTROL,
        })
    }

    #[test]
    fn empty_submit_is_rejected_by_default() {
        let mut ta = Textarea::new("Reply");
        assert!(ta.on(&ctrl_s()).is_none());
        assert!(ta.error.is_some());
    }

    #[test]
    fn allow_empty_submits_a_cleared_buffer() {
        // Notes editor: an emptied buffer submits so the daemon clears
        // the scratchpad (issue #458).
        let mut ta = Textarea::new("Notes").with_body("old note").allow_empty();
        // Clear the pre-filled body.
        for _ in 0.."old note".len() {
            ta.delete_back();
        }
        assert!(ta.buffer.is_empty());
        assert!(matches!(ta.on(&ctrl_s()), Some(Msg::TextareaSubmitted(b)) if b.is_empty()));
    }
}
