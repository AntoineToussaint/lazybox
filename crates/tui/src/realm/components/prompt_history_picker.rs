//! `PromptHistoryPicker` — per-session prompt history browser (issue
//! #523), opened with the terminal `]]h` leader.
//!
//! Lists every prompt the user has sent to the focused agent this
//! session, newest-first and timestamped, with snippet-sourced entries
//! tagged so it's obvious which came from the `]]s` picker. Typing
//! fuzzy-filters the rows; ↑/↓ navigate; Enter re-sends the chosen
//! prompt into the session.
//!
//! A row is a one-line summary of something that can run to paragraphs,
//! so the summary is never the only thing on offer (#1733): the
//! highlighted prompt's FULL text sits in a reader pane beside the list
//! (under it on a narrow terminal), scrollable to its final character
//! with PageUp/PageDown and Ctrl-u/Ctrl-d — reading never re-sends, which
//! stays deliberate on Enter. The filter matches that full text too, so a
//! word only present past the summary still finds its prompt.
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
    FilterModalChrome, FilterableList, PreviewPane, render_filter_modal, subsequence_icase,
};
use crate::realm::components::scrollable::handle_scroll_key;
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

/// Lines a PageUp/PageDown moves the reader pane. A fixed page (rather
/// than the laid-out height) keeps key handling independent of render;
/// `wrapped_window` clamps whatever it produces to the real viewport, so
/// paging past the end lands on the last screenful either way.
const PREVIEW_PAGE: u16 = 8;

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
    /// Topmost visible line of the reader pane, reset whenever the
    /// highlighted row changes so a new prompt opens at its first line.
    preview_scroll: u16,
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
            preview_scroll: 0,
        };
        picker.refilter();
        picker
    }

    pub fn on_key(&mut self, key: &KeyEvent) -> Option<Msg> {
        let before = self.selected();
        let msg = self.dispatch_key(key);
        if self.selected() != before {
            self.preview_scroll = 0;
        }
        msg
    }

    /// The highlighted prompt in full — what the reader pane shows, and
    /// the text Enter would re-send.
    fn selected_text(&self) -> Option<&str> {
        self.selected()
            .and_then(|i| self.texts.get(i))
            .map(String::as_str)
    }
}

impl FilterableList for PromptHistoryPicker {
    fn compute_visible(&mut self) -> Vec<usize> {
        let q = self.filter.trim();
        if q.is_empty() {
            (0..self.rows.len()).collect()
        } else {
            // Matched against the FULL prompt, not the row's one-line
            // summary (#1733) — a word that only appears in the third
            // paragraph still finds it.
            self.rows
                .iter()
                .enumerate()
                .filter_map(|(i, r)| {
                    let body = self.texts.get(i).map_or(r.text.as_str(), String::as_str);
                    let hay = match &r.tag {
                        Some(tag) => format!("{tag} {body}"),
                        None => body.to_string(),
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

    /// Page the reader pane. Only the keys the list protocol doesn't
    /// already claim reach here — PageUp/PageDown and Ctrl-u/Ctrl-d —
    /// so plain typing still filters and ↑/↓ still move the cursor.
    /// Scrolling is pure reading: it returns no message, so it can never
    /// re-send the prompt.
    fn custom_key(&mut self, key: &KeyEvent) -> Option<Msg> {
        let mut scroll = self.preview_scroll;
        if handle_scroll_key(&mut scroll, PREVIEW_PAGE, key) {
            self.preview_scroll = scroll;
        }
        None
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
            Span::styled("PgUp/PgDn", Style::default().fg(theme.accent).bold()),
            Span::raw(" read  "),
            Span::styled("Enter", Style::default().fg(theme.success).bold()),
            Span::raw(" re-send  "),
            Span::styled("Type", Style::default().fg(theme.accent).bold()),
            Span::raw(" filter  "),
            Span::styled("Esc", Style::default().fg(theme.error).bold()),
            Span::raw(" cancel"),
        ];
        // The reader pane: the highlighted prompt in full, so the row's
        // summary is never the last word on what Enter would send.
        let preview = self.selected_text().map(|text| PreviewPane {
            lines: text
                .lines()
                .map(|l| {
                    Line::from(Span::styled(
                        l.to_string(),
                        Style::default().fg(theme.text_strong),
                    ))
                })
                .collect(),
            scroll: self.preview_scroll,
        });
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
                preview,
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

    /// #1733: a word present only past the one-line summary still finds
    /// its prompt — the filter reads the full text, not the display row.
    #[test]
    fn filter_searches_past_the_summary() {
        let mut p = PromptHistoryPicker::new(rows());
        for c in "lease".chars() {
            assert!(p.on_key(&ke(c)).is_none());
        }
        // "force-push with lease" lives only in row 1's full text.
        assert_eq!(p.visible_indices, vec![1]);
        assert_eq!(
            p.selected_text(),
            Some("rebase onto main and force-push with lease"),
        );
    }

    /// Reading is not sending: paging through a long prompt moves the
    /// reader and emits nothing, and the payload is untouched.
    #[test]
    fn scrolling_the_preview_never_resends() {
        let long = (0..40)
            .map(|i| format!("line {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let mut p = PromptHistoryPicker::new(vec![(
            PromptRow {
                when: "now".into(),
                tag: None,
                text: "line 0 …".into(),
            },
            long.clone(),
        )]);

        assert!(p.on_key(&key(Key::PageDown)).is_none());
        assert!(p.preview_scroll > 0, "PageDown pages the reader");
        assert!(p.on_key(&key(Key::PageUp)).is_none());
        assert_eq!(p.preview_scroll, 0);
        assert!(
            p.on_key(&KeyEvent::new(Key::Char('d'), KeyModifiers::CONTROL))
                .is_none()
        );
        assert!(p.preview_scroll > 0, "Ctrl-d pages the reader");

        // The prompt itself is unchanged, and Enter still sends it whole.
        assert_eq!(p.selected_text(), Some(long.as_str()));
        assert!(matches!(
            p.on_key(&key(Key::Enter)),
            Some(Msg::ChoicePicked(_))
        ));
    }

    /// Moving the cursor opens the next prompt at its first line rather
    /// than inheriting the previous one's scroll position.
    #[test]
    fn moving_the_cursor_resets_the_reader() {
        let mut p = PromptHistoryPicker::new(rows());
        let _ = p.on_key(&key(Key::PageDown));
        assert!(p.preview_scroll > 0);
        let _ = p.on_key(&key(Key::Down));
        assert_eq!(p.preview_scroll, 0);
    }

    #[test]
    fn empty_picker_is_safe() {
        let mut p = PromptHistoryPicker::new(vec![]);
        assert!(p.cursor.is_none());
        assert!(p.on_key(&ke('x')).is_none());
        assert!(p.on_key(&key(Key::Enter)).is_none());
    }
}
