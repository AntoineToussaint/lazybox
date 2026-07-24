//! `PromptHistoryPicker` — per-session prompt history browser (issue
//! #523), opened with the terminal `]]h` leader.
//!
//! Lists every prompt the user has sent to the focused agent this
//! session, newest-first and timestamped, with snippet-sourced entries
//! tagged so it's obvious which came from the `]]s` picker. Typing
//! fuzzy-filters the rows (a case-insensitive subsequence match over the
//! prompt text + snippet tag); ↑/↓ navigate; Enter re-sends the chosen
//! prompt into the session.
//!
//! Modal returns:
//! - `Msg::ChoicePicked(vec![idx])` — index into the picker's row vec;
//!   the model holds the parallel `prompt_history_choices` texts.
//! - `Msg::ModalDismissed` — Esc or Ctrl-C.
//!
//! Like the jump picker (and unlike the snippet picker) this never
//! auto-submits: re-sending a prompt is a deliberate act, so the user
//! always confirms with Enter.

use crate::realm::Msg;
use crate::realm::UserEvent;
use tuirealm::command::{Cmd, CmdResult};
use tuirealm::component::{AppComponent, Component};
use tuirealm::event::{Event, Key, KeyEvent, KeyModifiers};
use tuirealm::props::{AttrValue, Attribute, QueryResult};
use tuirealm::ratatui::Frame;
use tuirealm::ratatui::layout::Rect;
use tuirealm::ratatui::prelude::*;
use tuirealm::ratatui::widgets::{Block, BorderType, Borders, Clear, Paragraph};
use tuirealm::state::State;

/// One history row for display. Pre-formatted by the model so the
/// component stays pure (no clock access): `when` is a relative age
/// ("2m ago"), `tag` is the snippet marker ("]rev") when the prompt came
/// from a snippet, and `text` is the single-line prompt summary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PromptRow {
    pub when: String,
    pub tag: Option<String>,
    pub text: String,
}

/// Case-insensitive subsequence test: do all chars of `needle` appear in
/// `haystack`, in order (gaps allowed)? Allocation-free — the filter
/// re-runs on every keystroke.
fn subsequence_icase(haystack: &str, needle: &str) -> bool {
    let mut hs = haystack.chars().map(|c| c.to_ascii_lowercase());
    'outer: for nc in needle.chars().map(|c| c.to_ascii_lowercase()) {
        for hc in hs.by_ref() {
            if hc == nc {
                continue 'outer;
            }
        }
        return false;
    }
    true
}

pub struct PromptHistoryPicker {
    /// Display rows, in the order the model stashed the parallel prompt
    /// texts. A picked index maps straight back to a text.
    rows: Vec<PromptRow>,
    /// Current filter string.
    filter: String,
    /// Cursor index into `visible_indices`. `None` when empty.
    cursor: Option<usize>,
    /// Indices into `rows` matching the filter, in display order.
    visible_indices: Vec<usize>,
}

impl PromptHistoryPicker {
    pub fn new(rows: Vec<PromptRow>) -> Self {
        let mut picker = Self {
            rows,
            filter: String::new(),
            cursor: None,
            visible_indices: Vec::new(),
        };
        picker.refilter();
        picker
    }

    fn refilter(&mut self) {
        let q = self.filter.trim();
        self.visible_indices = if q.is_empty() {
            (0..self.rows.len()).collect()
        } else {
            self.rows
                .iter()
                .enumerate()
                .filter_map(|(i, r)| {
                    let hay = match &r.tag {
                        Some(tag) => format!("{} {}", tag, r.text),
                        None => r.text.clone(),
                    };
                    subsequence_icase(&hay, q).then_some(i)
                })
                .collect()
        };
        self.cursor = (!self.visible_indices.is_empty()).then_some(0);
    }

    pub fn on_key(&mut self, key: &KeyEvent) -> Option<Msg> {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        if matches!(key.code, Key::Esc) || (ctrl && matches!(key.code, Key::Char('c'))) {
            return Some(Msg::ModalDismissed);
        }
        match key.code {
            Key::Down => {
                if let Some(c) = self.cursor
                    && c + 1 < self.visible_indices.len()
                {
                    self.cursor = Some(c + 1);
                }
                None
            }
            Key::Up => {
                if let Some(c) = self.cursor
                    && c > 0
                {
                    self.cursor = Some(c - 1);
                }
                None
            }
            Key::Home => {
                if !self.visible_indices.is_empty() {
                    self.cursor = Some(0);
                }
                None
            }
            Key::End => {
                if !self.visible_indices.is_empty() {
                    self.cursor = Some(self.visible_indices.len() - 1);
                }
                None
            }
            Key::Enter => {
                let c = self.cursor?;
                let row_idx = *self.visible_indices.get(c)?;
                Some(Msg::ChoicePicked(vec![row_idx]))
            }
            Key::Backspace => {
                self.filter.pop();
                self.refilter();
                None
            }
            Key::Char(c) if !ctrl => {
                self.filter.push(c);
                self.refilter();
                None
            }
            _ => None,
        }
    }
}

impl Component for PromptHistoryPicker {
    fn view(&mut self, frame: &mut Frame, area: Rect) {
        let theme = crate::theme::current();
        let modal_w = 88u16.min(area.width.saturating_sub(4));
        let modal_h = 24u16.min(area.height.saturating_sub(4));
        let x = area.x + area.width.saturating_sub(modal_w) / 2;
        let y = area.y + area.height.saturating_sub(modal_h) / 2;
        let modal = Rect::new(x, y, modal_w, modal_h);

        frame.render_widget(Clear, modal);
        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .title(Span::styled(" Prompt history ", theme.modal_title()))
            .border_style(theme.modal_border());
        let inner = block.inner(modal);
        frame.render_widget(block, modal);

        if inner.height < 4 {
            return;
        }
        let filter_rect = Rect {
            x: inner.x,
            y: inner.y,
            width: inner.width,
            height: 1,
        };
        let div_rect = Rect {
            x: inner.x,
            y: inner.y + 1,
            width: inner.width,
            height: 1,
        };
        let help_rect = Rect {
            x: inner.x,
            y: inner.y + inner.height - 1,
            width: inner.width,
            height: 1,
        };
        let body_rect = Rect {
            x: inner.x,
            y: inner.y + 2,
            width: inner.width,
            height: inner.height - 3,
        };

        let filter_line = Line::from(vec![
            Span::styled("› ", Style::default().fg(theme.accent).bold()),
            Span::styled(self.filter.clone(), Style::default().fg(theme.text_strong)),
            Span::styled("▌", Style::default().fg(theme.accent)),
        ]);
        frame.render_widget(Paragraph::new(filter_line), filter_rect);

        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                "─".repeat(inner.width as usize),
                theme.divider(),
            ))),
            div_rect,
        );

        // Scroll the visible window so the cursor stays on screen.
        let rows = body_rect.height as usize;
        let cursor = self.cursor.unwrap_or(0);
        let start = cursor.saturating_sub(rows.saturating_sub(1));
        let mut body: Vec<Line> = Vec::with_capacity(rows.max(1));
        if self.visible_indices.is_empty() {
            body.push(Line::from(Span::styled(
                if self.rows.is_empty() {
                    "  (no prompts sent yet)"
                } else {
                    "  (no matches)"
                },
                Style::default().fg(theme.text_dim).italic(),
            )));
        } else {
            for (i, &row_idx) in self
                .visible_indices
                .iter()
                .enumerate()
                .skip(start)
                .take(rows)
            {
                let row = &self.rows[row_idx];
                let is_cursor = self.cursor == Some(i);
                let caret = if is_cursor { "▸ " } else { "  " };
                let base = if is_cursor {
                    Style::default()
                        .fg(theme.text_strong)
                        .bg(theme.fill)
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::default().fg(theme.text_strong)
                };
                let dim = if is_cursor {
                    base.fg(theme.text_dim)
                } else {
                    Style::default().fg(theme.text_dim)
                };
                let mut spans = vec![
                    Span::styled(caret.to_string(), base),
                    Span::styled(format!("{:>8}  ", row.when), dim),
                ];
                if let Some(tag) = &row.tag {
                    spans.push(Span::styled(
                        format!("{tag} "),
                        if is_cursor {
                            base.fg(theme.accent)
                        } else {
                            Style::default().fg(theme.accent)
                        },
                    ));
                }
                spans.push(Span::styled(row.text.clone(), base));
                body.push(Line::from(spans));
            }
        }
        frame.render_widget(Paragraph::new(body), body_rect);

        let help_spans = vec![
            Span::styled("↑↓", Style::default().fg(theme.accent).bold()),
            Span::raw(" navigate  "),
            Span::styled("Enter", Style::default().fg(theme.success).bold()),
            Span::raw(" re-send  "),
            Span::styled("Type", Style::default().fg(theme.accent).bold()),
            Span::raw(" filter  "),
            Span::styled("Esc", Style::default().fg(theme.error).bold()),
            Span::raw(" cancel"),
        ];
        frame.render_widget(Paragraph::new(Line::from(help_spans)), help_rect);
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

impl AppComponent<Msg, UserEvent> for PromptHistoryPicker {
    fn on(&mut self, ev: &Event<UserEvent>) -> Option<Msg> {
        match ev {
            Event::Keyboard(key) => self.on_key(key),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ke(c: char) -> KeyEvent {
        KeyEvent::new(Key::Char(c), KeyModifiers::NONE)
    }
    fn key(code: Key) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }
    fn rows() -> Vec<PromptRow> {
        vec![
            PromptRow {
                when: "just now".into(),
                tag: Some("]rev".into()),
                text: "review the diff".into(),
            },
            PromptRow {
                when: "2m ago".into(),
                tag: None,
                text: "rebase onto main".into(),
            },
            PromptRow {
                when: "5m ago".into(),
                tag: None,
                text: "run the tests".into(),
            },
        ]
    }

    #[test]
    fn empty_filter_shows_all_rows() {
        let p = PromptHistoryPicker::new(rows());
        assert_eq!(p.visible_indices, vec![0, 1, 2]);
        assert_eq!(p.cursor, Some(0));
    }

    #[test]
    fn filter_matches_text_and_snippet_tag() {
        // "rev" is a subsequence of the snippet tag on row 0.
        let mut p = PromptHistoryPicker::new(rows());
        for c in ['r', 'e', 'v'] {
            assert!(p.on_key(&ke(c)).is_none());
        }
        assert!(p.visible_indices.contains(&0));
    }

    #[test]
    fn enter_submits_the_cursor_row() {
        let mut p = PromptHistoryPicker::new(rows());
        let _ = p.on_key(&key(Key::Down));
        match p.on_key(&key(Key::Enter)) {
            Some(Msg::ChoicePicked(v)) => assert_eq!(v, vec![1]),
            other => panic!("expected ChoicePicked, got {other:?}"),
        }
    }

    #[test]
    fn typing_never_auto_submits() {
        let mut p = PromptHistoryPicker::new(rows());
        for c in "run the tests".chars() {
            assert!(p.on_key(&ke(c)).is_none());
        }
        assert_eq!(p.visible_indices, vec![2]);
    }

    #[test]
    fn esc_and_ctrl_c_dismiss() {
        let mut p = PromptHistoryPicker::new(rows());
        assert!(matches!(
            p.on_key(&key(Key::Esc)),
            Some(Msg::ModalDismissed)
        ));
        let mut p = PromptHistoryPicker::new(rows());
        let ev = KeyEvent::new(Key::Char('c'), KeyModifiers::CONTROL);
        assert!(matches!(p.on_key(&ev), Some(Msg::ModalDismissed)));
    }

    #[test]
    fn empty_picker_is_safe() {
        let mut p = PromptHistoryPicker::new(vec![]);
        assert!(p.cursor.is_none());
        assert!(p.on_key(&ke('x')).is_none());
        assert!(p.on_key(&key(Key::Enter)).is_none());
    }
}
