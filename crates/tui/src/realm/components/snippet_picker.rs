//! `SnippetPicker` — fuzzy-filter picker for the snippet system.
//!
//! Combines a single-line text input (the filter) with a list of
//! matching snippets. Typing extends the filter; ↑/↓ navigate;
//! Enter expands the selected snippet AND auto-submits (the
//! caller's `handle_choice_picked` arm writes `body + "\r"` to
//! the active terminal). The "expand and submit" pair is the
//! whole point of the feature — see issue #40.
//!
//! Modal returns:
//! - `Msg::ChoicePicked(vec![idx])` — index into the picker's
//!   row vec (NOT into the visible-after-filter subset). The
//!   model snapshots the same row vec in `snippet_choices`, so
//!   the handler resolves idx → key with a single index.
//! - `Msg::ModalDismissed` — Esc or Ctrl-C.
//!
//! Open-with-filter UX: the model arms a one-keystroke latch on
//! `]` inside the terminal pane (sibling to the existing `]]`
//! escape latch). When the next key is a printable char, the
//! model mounts this picker with `initial_filter = that char` so
//! `]rev` flows as `]` → mount → filter=`r` → filter=`re` →
//! filter=`rev`. If the filter ever equals a snippet KEY exactly
//! and that key is the only match, the picker auto-submits — the
//! 3-keystroke (`]rev`) → submit ergonomics from the issue.

use crate::realm::Msg;
use crate::realm::UserEvent;
use pilot_config::Snippet;
use tuirealm::command::{Cmd, CmdResult};
use tuirealm::component::{AppComponent, Component};
use tuirealm::event::{Event, Key, KeyEvent, KeyModifiers};
use tuirealm::props::{AttrValue, Attribute, QueryResult};
use tuirealm::ratatui::Frame;
use tuirealm::ratatui::layout::Rect;
use tuirealm::ratatui::prelude::*;
use tuirealm::ratatui::widgets::{Block, BorderType, Borders, Clear, Paragraph};
use tuirealm::state::State;

/// A single picker row — snippet shortcut key plus the underlying
/// definition. The picker stores rows as `(key, snippet)` tuples
/// so the filter/render path doesn't need a second lookup.
#[derive(Clone)]
pub struct PickerRow {
    pub key: String,
    pub description: String,
    pub body_preview: String,
    pub origin: pilot_config::SnippetOrigin,
}

impl PickerRow {
    pub fn from(key: &str, snippet: &Snippet) -> Self {
        let preview = snippet
            .body
            .lines()
            .find(|l| !l.trim().is_empty())
            .unwrap_or("")
            .trim()
            .to_string();
        Self {
            key: key.to_string(),
            description: snippet.description.clone(),
            body_preview: preview,
            origin: snippet.origin,
        }
    }
}

pub struct SnippetPicker {
    rows: Vec<PickerRow>,
    filter: String,
    cursor: usize,
    visible_indices: Vec<usize>,
}

impl SnippetPicker {
    pub fn new(mut rows: Vec<PickerRow>, initial_filter: String) -> Self {
        // Sort by key so the picker presents a stable, predictable
        // order regardless of how the caller assembled the Vec.
        // `Snippets::entries` already walks a BTreeMap (alphabetic
        // by key) so this is a no-op in the production path; tests
        // and any future synthetic-input callers don't have to
        // remember to pre-sort.
        rows.sort_by(|a, b| a.key.cmp(&b.key));
        let mut picker = Self {
            rows,
            filter: initial_filter,
            cursor: 0,
            visible_indices: Vec::new(),
        };
        picker.refilter();
        picker
    }

    pub fn visible_indices(&self) -> &[usize] {
        &self.visible_indices
    }

    pub fn filter_text(&self) -> &str {
        &self.filter
    }

    pub fn rows(&self) -> &[PickerRow] {
        &self.rows
    }

    /// Recompute `visible_indices` from `filter` and clamp cursor.
    /// Prefix match on the snippet key, case-insensitive. Description
    /// is shown to help the user pick, but not searched — a one-char
    /// query would otherwise match almost every snippet through some
    /// stray letter in a description, drowning out the key-prefix
    /// hits the user is actually after. The `]<key>` ergonomics
    /// (issue #40) want predictable "type the prefix, top hit is
    /// what you want" behaviour, so prefix-only is the right shape.
    fn refilter(&mut self) {
        let q = self.filter.trim().to_ascii_lowercase();
        if q.is_empty() {
            self.visible_indices = (0..self.rows.len()).collect();
        } else {
            self.visible_indices = self
                .rows
                .iter()
                .enumerate()
                .filter_map(|(i, r)| {
                    if r.key.to_ascii_lowercase().starts_with(&q) {
                        Some(i)
                    } else {
                        None
                    }
                })
                .collect();
        }
        if self.cursor >= self.visible_indices.len() {
            self.cursor = self.visible_indices.len().saturating_sub(1);
        }
    }

    /// `Some(idx)` when the picker should auto-submit: the typed
    /// filter exactly equals a snippet key AND that snippet is the
    /// sole visible match. Anything else (multiple matches, partial
    /// matches, no exact key hit) → user must press Enter.
    fn auto_submit_index(&self) -> Option<usize> {
        if self.visible_indices.len() != 1 {
            return None;
        }
        let idx = self.visible_indices[0];
        let key = &self.rows[idx].key;
        if key.eq_ignore_ascii_case(self.filter.trim()) {
            Some(idx)
        } else {
            None
        }
    }
}

impl Component for SnippetPicker {
    fn view(&mut self, frame: &mut Frame, area: Rect) {
        let theme = crate::theme::current();
        let modal_w = 80u16.min(area.width.saturating_sub(4));
        let modal_h = 24u16.min(area.height.saturating_sub(4));
        let x = area.x + area.width.saturating_sub(modal_w) / 2;
        let y = area.y + area.height.saturating_sub(modal_h) / 2;
        let modal = Rect::new(x, y, modal_w, modal_h);

        frame.render_widget(Clear, modal);
        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .title(Span::styled(" Snippets ", theme.modal_title()))
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
            Span::styled("] ", Style::default().fg(theme.accent).bold()),
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

        let mut body: Vec<Line> = Vec::with_capacity(self.visible_indices.len());
        if self.visible_indices.is_empty() {
            body.push(Line::from(Span::styled(
                if self.rows.is_empty() {
                    "  (no snippets configured — see ~/.pilot/snippets.yaml)"
                } else {
                    "  (no matches)"
                },
                Style::default().fg(theme.text_dim).italic(),
            )));
        } else {
            for (i, &row_idx) in self.visible_indices.iter().enumerate() {
                let r = &self.rows[row_idx];
                let is_cursor = i == self.cursor;
                let caret = if is_cursor { "▸ " } else { "  " };
                let mut row_style = Style::default().fg(theme.text_strong);
                if is_cursor {
                    row_style = row_style.bg(theme.fill).add_modifier(Modifier::BOLD);
                }
                let origin_tag = r.origin.label();
                let mut spans: Vec<Span<'static>> = vec![
                    Span::styled(caret.to_string(), row_style),
                    Span::styled(
                        format!("]{:<6}  ", r.key),
                        row_style.fg(theme.accent),
                    ),
                    Span::styled(r.description.clone(), row_style),
                ];
                if !origin_tag.is_empty() {
                    spans.push(Span::raw("  "));
                    spans.push(Span::styled(
                        format!("[{origin_tag}]"),
                        Style::default().fg(theme.text_dim).italic(),
                    ));
                }
                if !r.body_preview.is_empty() {
                    spans.push(Span::raw("  · "));
                    spans.push(Span::styled(
                        r.body_preview.clone(),
                        Style::default().fg(theme.text_dim),
                    ));
                }
                body.push(Line::from(spans));
            }
        }
        frame.render_widget(Paragraph::new(body), body_rect);

        let help_spans = vec![
            Span::styled("↑↓", Style::default().fg(theme.accent).bold()),
            Span::raw(" navigate  "),
            Span::styled("Enter", Style::default().fg(theme.success).bold()),
            Span::raw(" send  "),
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

impl AppComponent<Msg, UserEvent> for SnippetPicker {
    fn on(&mut self, ev: &Event<UserEvent>) -> Option<Msg> {
        let Event::Keyboard(key) = ev else {
            return None;
        };
        handle_picker_key(self, key)
    }
}

/// Pure key handler factored out so tests can drive the picker
/// without spinning up a tuirealm `Application`.
pub fn handle_picker_key(picker: &mut SnippetPicker, key: &KeyEvent) -> Option<Msg> {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    if matches!(key.code, Key::Esc) || (ctrl && matches!(key.code, Key::Char('c'))) {
        return Some(Msg::ModalDismissed);
    }
    match key.code {
        Key::Down => {
            if !picker.visible_indices.is_empty()
                && picker.cursor + 1 < picker.visible_indices.len()
            {
                picker.cursor += 1;
            }
            None
        }
        Key::Up => {
            if picker.cursor > 0 {
                picker.cursor -= 1;
            }
            None
        }
        Key::Home => {
            picker.cursor = 0;
            None
        }
        Key::End => {
            if !picker.visible_indices.is_empty() {
                picker.cursor = picker.visible_indices.len() - 1;
            }
            None
        }
        Key::Enter => {
            picker
                .visible_indices
                .get(picker.cursor)
                .map(|&row_idx| Msg::ChoicePicked(vec![row_idx]))
        }
        Key::Backspace => {
            picker.filter.pop();
            picker.refilter();
            picker.cursor = 0;
            None
        }
        Key::Char(c) if !ctrl => {
            picker.filter.push(c);
            picker.refilter();
            picker.cursor = 0;
            // Auto-submit when the filter equals a key uniquely.
            picker
                .auto_submit_index()
                .map(|idx| Msg::ChoicePicked(vec![idx]))
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pilot_config::{Snippet, SnippetOrigin};

    fn make_rows() -> Vec<PickerRow> {
        vec![
            PickerRow::from(
                "rev",
                &Snippet {
                    description: "Review diff".into(),
                    body: "review please".into(),
                    origin: SnippetOrigin::Global,
                },
            ),
            PickerRow::from(
                "pr",
                &Snippet {
                    description: "Open PR".into(),
                    body: "open pr please".into(),
                    origin: SnippetOrigin::Global,
                },
            ),
            PickerRow::from(
                "deploy",
                &Snippet {
                    description: "Pre-deploy check".into(),
                    body: "deploy check".into(),
                    origin: SnippetOrigin::Repo,
                },
            ),
        ]
    }

    fn ke(c: char) -> KeyEvent {
        KeyEvent::new(Key::Char(c), KeyModifiers::NONE)
    }

    #[test]
    fn empty_filter_shows_all_rows_in_key_order() {
        let picker = SnippetPicker::new(make_rows(), String::new());
        let keys: Vec<_> = picker
            .visible_indices
            .iter()
            .map(|&i| picker.rows[i].key.clone())
            .collect();
        assert_eq!(keys, vec!["deploy", "pr", "rev"]);
    }

    #[test]
    fn typing_filters_and_recovers_on_backspace() {
        let mut picker = SnippetPicker::new(make_rows(), String::new());
        let out = handle_picker_key(&mut picker, &ke('r'));
        assert!(out.is_none(), "no auto-submit on incomplete key");
        assert_eq!(picker.visible_indices.len(), 1);
        assert_eq!(picker.rows[picker.visible_indices[0]].key, "rev");

        let _ = handle_picker_key(
            &mut picker,
            &KeyEvent::new(Key::Backspace, KeyModifiers::NONE),
        );
        assert_eq!(picker.filter, "");
        assert_eq!(picker.visible_indices.len(), 3);
    }

    /// Headline test: typing a full snippet key auto-submits.
    /// `]rev` (where `r`, `e`, `v` are streamed in) should land
    /// on `ChoicePicked` after the `v` — no Enter.
    #[test]
    fn typing_full_key_auto_submits() {
        let mut picker = SnippetPicker::new(make_rows(), String::new());
        assert!(handle_picker_key(&mut picker, &ke('r')).is_none());
        assert!(handle_picker_key(&mut picker, &ke('e')).is_none());
        let out = handle_picker_key(&mut picker, &ke('v'));
        match out {
            Some(Msg::ChoicePicked(v)) => {
                assert_eq!(v.len(), 1);
                assert_eq!(picker.rows[v[0]].key, "rev");
            }
            other => panic!("expected ChoicePicked, got {other:?}"),
        }
    }

    /// Mounted with an initial filter (the `]r` flow). The initial
    /// `r` is already in the picker's filter; the next chars
    /// continue from there.
    #[test]
    fn initial_filter_carries_through() {
        let mut picker = SnippetPicker::new(make_rows(), "r".into());
        assert_eq!(picker.visible_indices.len(), 1);
        assert!(handle_picker_key(&mut picker, &ke('e')).is_none());
        match handle_picker_key(&mut picker, &ke('v')) {
            Some(Msg::ChoicePicked(v)) => assert_eq!(picker.rows[v[0]].key, "rev"),
            other => panic!("expected ChoicePicked, got {other:?}"),
        }
    }

    /// Enter on the cursor submits even when the filter prefixes
    /// multiple keys (the discovery / manual-pick path).
    #[test]
    fn enter_submits_cursor_selection() {
        let mut rows = make_rows();
        rows.push(PickerRow::from(
            "ping",
            &Snippet {
                description: "Ping".into(),
                body: "ping body".into(),
                origin: SnippetOrigin::Global,
            },
        ));
        let mut picker = SnippetPicker::new(rows, String::new());
        assert!(handle_picker_key(&mut picker, &ke('p')).is_none());
        assert_eq!(picker.visible_indices.len(), 2);
        let _ = handle_picker_key(
            &mut picker,
            &KeyEvent::new(Key::Down, KeyModifiers::NONE),
        );
        match handle_picker_key(
            &mut picker,
            &KeyEvent::new(Key::Enter, KeyModifiers::NONE),
        ) {
            Some(Msg::ChoicePicked(v)) => assert_eq!(picker.rows[v[0]].key, "pr"),
            other => panic!("expected ChoicePicked, got {other:?}"),
        }
    }

    #[test]
    fn esc_dismisses() {
        let mut picker = SnippetPicker::new(make_rows(), String::new());
        let out = handle_picker_key(&mut picker, &KeyEvent::new(Key::Esc, KeyModifiers::NONE));
        assert!(matches!(out, Some(Msg::ModalDismissed)));
    }

    #[test]
    fn picker_with_no_snippets_handles_typing_safely() {
        let mut picker = SnippetPicker::new(vec![], String::new());
        assert!(handle_picker_key(&mut picker, &ke('x')).is_none());
        assert!(picker.visible_indices.is_empty());
        assert!(
            handle_picker_key(
                &mut picker,
                &KeyEvent::new(Key::Enter, KeyModifiers::NONE)
            )
            .is_none()
        );
    }
}
