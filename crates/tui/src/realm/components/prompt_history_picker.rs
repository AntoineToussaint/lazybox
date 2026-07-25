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
//! - `Msg::ChoicePicked(vec![ChoicePayload::Text(text)])` — the full
//!   prompt text to re-send, carried on the row itself so a filtered /
//!   re-ordered display can't resolve to the wrong prompt (issue #512).
//!   No parallel model-side stash.
//! - `Msg::ModalDismissed` — Esc or Ctrl-C.
//!
//! Like the jump picker (and unlike the snippet picker) this never
//! auto-submits: re-sending a prompt is a deliberate act, so the user
//! always confirms with Enter.

use crate::realm::ChoicePayload;
use crate::realm::Msg;
use crate::realm::UserEvent;
use crate::realm::components::filterable::{
    FilterModalChrome, FilterableList, render_filter_modal, subsequence_icase,
};
use tuirealm::command::{Cmd, CmdResult};
use tuirealm::component::{AppComponent, Component};
use tuirealm::event::{Event, KeyEvent};
#[cfg(test)]
use tuirealm::event::{Key, KeyModifiers};
use tuirealm::props::{AttrValue, Attribute, QueryResult};
use tuirealm::ratatui::Frame;
use tuirealm::ratatui::layout::Rect;
use tuirealm::ratatui::prelude::*;
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

pub struct PromptHistoryPicker {
    /// Display rows, index-aligned with [`Self::texts`]. Built together
    /// and never re-ordered, so the filtered display can't desync them.
    rows: Vec<PromptRow>,
    /// The full prompt text each row re-sends (same length / order as
    /// `rows`). The display `PromptRow::text` is a truncated summary, so
    /// the resend value must travel separately; the picked row reports it
    /// as its [`ChoicePayload::Text`].
    texts: Vec<String>,
    /// Current filter string.
    filter: String,
    /// Cursor index into `visible_indices`. `None` when empty.
    cursor: Option<usize>,
    /// Indices into `rows` matching the filter, in display order.
    visible_indices: Vec<usize>,
}

impl PromptHistoryPicker {
    /// `rows` pairs each display row with the full prompt text it
    /// re-sends. The pairing travels through the picker so Enter always
    /// resolves to the full text of the row the user highlighted.
    pub fn new(rows: Vec<(PromptRow, String)>) -> Self {
        let (rows, texts): (Vec<_>, Vec<_>) = rows.into_iter().unzip();
        let mut picker = Self {
            rows,
            texts,
            filter: String::new(),
            cursor: None,
            visible_indices: Vec::new(),
        };
        picker.refilter();
        picker
    }

    pub fn on_key(&mut self, key: &KeyEvent) -> Option<Msg> {
        self.dispatch_key(key)
    }
}

impl FilterableList for PromptHistoryPicker {
    fn compute_visible(&mut self) -> Vec<usize> {
        let q = self.filter.trim();
        if q.is_empty() {
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
        }
    }

    fn pick(&self, item_idx: usize) -> Option<Msg> {
        let text = self.texts.get(item_idx)?.clone();
        Some(Msg::ChoicePicked(vec![ChoicePayload::Text(text)]))
    }

    fn filter(&self) -> &str {
        &self.filter
    }
    fn filter_mut(&mut self) -> &mut String {
        &mut self.filter
    }
    fn cursor(&self) -> Option<usize> {
        self.cursor
    }
    fn set_cursor(&mut self, cursor: Option<usize>) {
        self.cursor = cursor;
    }
    fn visible(&self) -> &[usize] {
        &self.visible_indices
    }
    fn set_visible(&mut self, visible: Vec<usize>) {
        self.visible_indices = visible;
    }
}

impl Component for PromptHistoryPicker {
    fn view(&mut self, frame: &mut Frame, area: Rect) {
        let theme = crate::theme::current();
        let empty = if self.rows.is_empty() {
            "  (no prompts sent yet)"
        } else {
            "  (no matches)"
        };
        let help = vec![
            Span::styled("↑↓", Style::default().fg(theme.accent).bold()),
            Span::raw(" navigate  "),
            Span::styled("Enter", Style::default().fg(theme.success).bold()),
            Span::raw(" re-send  "),
            Span::styled("Type", Style::default().fg(theme.accent).bold()),
            Span::raw(" filter  "),
            Span::styled("Esc", Style::default().fg(theme.error).bold()),
            Span::raw(" cancel"),
        ];
        render_filter_modal(
            self,
            frame,
            area,
            theme,
            FilterModalChrome {
                title: " Prompt history ",
                modal_w: 88,
                empty,
                help,
            },
            |row_idx, is_cursor| {
                let row = &self.rows[row_idx];
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
                Line::from(spans)
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
    fn rows() -> Vec<(PromptRow, String)> {
        vec![
            (
                PromptRow {
                    when: "just now".into(),
                    tag: Some("]rev".into()),
                    text: "review the diff".into(),
                },
                "review the diff".into(),
            ),
            (
                PromptRow {
                    when: "2m ago".into(),
                    tag: None,
                    // Display summary is truncated; the full resend text
                    // differs, so the payload must carry the full text.
                    text: "rebase onto main".into(),
                },
                "rebase onto main and force-push with lease".into(),
            ),
            (
                PromptRow {
                    when: "5m ago".into(),
                    tag: None,
                    text: "run the tests".into(),
                },
                "run the tests".into(),
            ),
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
    fn enter_submits_the_cursor_rows_full_text() {
        let mut p = PromptHistoryPicker::new(rows());
        let _ = p.on_key(&key(Key::Down));
        match p.on_key(&key(Key::Enter)) {
            // Row 1's *full* resend text — not its truncated summary,
            // and not a bare index.
            Some(Msg::ChoicePicked(v)) => assert_eq!(
                v,
                vec![ChoicePayload::Text(
                    "rebase onto main and force-push with lease".into()
                )]
            ),
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
