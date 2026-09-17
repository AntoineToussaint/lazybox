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
    FilterModalChrome, FilterableList, PreviewPane, contains_icase, render_filter_modal,
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
            // paragraph still finds it. Substring, not subsequence: over a
            // multi-paragraph body a gapped match returns almost every row,
            // so widening the corpus would have cost the filter the
            // selectivity it exists for. Neither side is copied.
            self.rows
                .iter()
                .enumerate()
                .filter_map(|(i, r)| {
                    let body = self.texts.get(i).map_or(r.text.as_str(), String::as_str);
                    let tag_hit = r.tag.as_deref().is_some_and(|t| contains_icase(t, q));
                    (tag_hit || contains_icase(body, q)).then_some(i)
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
        // The reading keys are advertised by the chrome only when it
        // actually lays a reader out (#1733) — on a modal too short for
        // one they would name keys that move nothing.
        let preview_help = vec![
            Span::styled("PgUp/PgDn", Style::default().fg(theme.accent).bold()),
            Span::raw(" read"),
        ];
        let help = vec![
            Span::styled("↑↓", Style::default().fg(theme.accent).bold()),
            Span::raw(" navigate  "),
            Span::styled("Enter", Style::default().fg(theme.success).bold()),
            Span::raw(" re-send  "),
            Span::styled("Type", Style::default().fg(theme.accent).bold()),
            Span::raw(" filter  "),
            Span::styled("Esc", Style::default().fg(theme.error).bold()),
            Span::raw(" cancel  "),
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
        let clamped = render_filter_modal(
            self,
            frame,
            area,
            theme,
            FilterModalChrome {
                title: " Prompt history ",
                modal_w: 88,
                empty,
                help,
                preview_help,
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
        // Adopt the offset the reader actually settled on, so paging past
        // the end doesn't bank a runaway scroll that swallows the next
        // several PageUps.
        if let Some(scroll) = clamped {
            self.preview_scroll = scroll;
        }
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

    /// Paging past the end must not bank a runaway offset: the reader
    /// clamps to the last screenful, and the picker adopts that clamped
    /// value, so the very next PageUp moves the view. Before the
    /// write-back, ten PageDowns on a short prompt left `preview_scroll`
    /// at 80 and swallowed the next nine PageUps.
    #[test]
    fn overscrolling_does_not_swallow_the_next_page_up() {
        use tuirealm::ratatui::Terminal;
        use tuirealm::ratatui::backend::TestBackend;
        let body = (0..30)
            .map(|i| format!("L{i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let mut p = PromptHistoryPicker::new(vec![(
            PromptRow {
                when: "now".into(),
                tag: None,
                text: "L0 …".into(),
            },
            body,
        )]);
        let render = |p: &mut PromptHistoryPicker| {
            let mut t = Terminal::new(TestBackend::new(100, 30)).expect("terminal");
            t.draw(|f| p.view(f, Rect::new(0, 0, 100, 30)))
                .expect("draw");
            let buf = t.backend().buffer().clone();
            (0..30)
                .map(|y| (0..100).map(|x| buf[(x, y)].symbol()).collect::<String>())
                .collect::<Vec<_>>()
                .join("\n")
        };

        for _ in 0..10 {
            let _ = p.on_key(&key(Key::PageDown));
        }
        let bottom = render(&mut p);
        assert!(bottom.contains("L29"), "never reached the end:\n{bottom}");
        let settled = p.preview_scroll;
        assert!(settled < 30, "runaway offset banked: {settled}");

        let _ = p.on_key(&key(Key::PageUp));
        let after = render(&mut p);
        assert_ne!(bottom, after, "one PageUp moved nothing:\n{after}");
    }

    /// #1733 asks for a reading path on a small terminal, and an 80x20
    /// one is small: the modal's body is 11 rows there. A fixed split
    /// dropped the reader entirely at that size — silently, while the
    /// help line went on naming its scroll keys. The reader now shrinks
    /// to fit, and the hint appears only when one is laid out.
    #[test]
    fn a_small_terminal_still_gets_a_reader_and_an_honest_help_line() {
        use tuirealm::ratatui::Terminal;
        use tuirealm::ratatui::backend::TestBackend;
        let body = (0..30)
            .map(|i| format!("body line {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let mut p = PromptHistoryPicker::new(vec![(
            PromptRow {
                when: "now".into(),
                tag: None,
                text: "body line 0 …".into(),
            },
            body,
        )]);
        let render = |p: &mut PromptHistoryPicker, w: u16, h: u16| {
            let mut t = Terminal::new(TestBackend::new(w, h)).expect("terminal");
            t.draw(|f| p.view(f, Rect::new(0, 0, w, h))).expect("draw");
            let buf = t.backend().buffer().clone();
            (0..h)
                .map(|y| (0..w).map(|x| buf[(x, y)].symbol()).collect::<String>())
                .collect::<Vec<_>>()
                .join("\n")
        };

        let small = render(&mut p, 80, 20);
        assert!(
            small.contains("body line 0"),
            "no reader on a small terminal:\n{small}",
        );
        assert!(small.contains("read"), "reader hint missing:\n{small}");
        // …and it still reaches the end from there.
        for _ in 0..10 {
            let _ = p.on_key(&key(Key::PageDown));
        }
        let paged = render(&mut p, 80, 20);
        assert!(paged.contains("body line 29"), "tail unreachable:\n{paged}");

        // A modal with no room for a reader says so by omission: the
        // scroll keys are not advertised.
        let tiny = render(&mut p, 60, 12);
        assert!(!tiny.contains("read"), "hint outlived the reader:\n{tiny}");
    }

    /// Widening the corpus to the full prompt must not cost the filter its
    /// selectivity: a gapped (subsequence) match over paragraphs hits
    /// almost everything, so the match is a substring.
    #[test]
    fn full_text_search_still_discriminates() {
        let mut p = PromptHistoryPicker::new(vec![
            (
                PromptRow {
                    when: "a".into(),
                    tag: None,
                    text: "Please rebase…".into(),
                },
                "Please rebase onto main and resolve the conflicts in the parser".into(),
            ),
            (
                PromptRow {
                    when: "b".into(),
                    tag: None,
                    text: "Investigate…".into(),
                },
                "Investigate the flaky test under load and report what you find".into(),
            ),
        ]);
        // `tin` is a subsequence of BOTH bodies and a substring of neither.
        for c in "tin".chars() {
            let _ = p.on_key(&ke(c));
        }
        assert!(p.visible_indices.is_empty(), "gapped match resurfaced");
        for _ in 0..3 {
            let _ = p.on_key(&key(Key::Backspace));
        }
        for c in "parser".chars() {
            let _ = p.on_key(&ke(c));
        }
        assert_eq!(p.visible_indices, vec![0], "a real word past the summary");
    }

    #[test]
    fn empty_picker_is_safe() {
        let mut p = PromptHistoryPicker::new(vec![]);
        assert!(p.cursor.is_none());
        assert!(p.on_key(&ke('x')).is_none());
        assert!(p.on_key(&key(Key::Enter)).is_none());
    }
}
