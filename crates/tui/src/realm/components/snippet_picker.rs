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
//!   model snapshots the same row keys in `snippet_choices`, so
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

/// A single picker row — snippet shortcut key plus the bits the
/// picker needs to render it. The full `Snippet` (with its body)
/// stays on the model; the picker only needs a label.
#[derive(Clone, Debug)]
pub struct PickerRow {
    pub key: String,
    pub description: String,
    pub body_preview: String,
    pub origin: pilot_config::SnippetOrigin,
}

impl PickerRow {
    /// Build a row from a snippet entry. Body preview = first
    /// non-empty line, trimmed — keeps multi-line snippet bodies
    /// from blowing up the 1-row picker entry while the full
    /// body still flows to the agent on submit.
    pub fn new(key: &str, snippet: &Snippet) -> Self {
        let body_preview = snippet
            .body
            .lines()
            .find(|l| !l.trim().is_empty())
            .unwrap_or("")
            .trim()
            .to_string();
        Self {
            key: key.to_string(),
            description: snippet.description.clone(),
            body_preview,
            origin: snippet.origin,
        }
    }
}

/// Case-insensitive ASCII prefix check that doesn't allocate. The
/// snippet picker re-runs the filter on every keystroke; the
/// previous `to_ascii_lowercase()` approach was allocating one
/// `String` per row per keystroke.
fn starts_with_icase(haystack: &str, needle: &str) -> bool {
    let h = haystack.as_bytes();
    let n = needle.as_bytes();
    h.len() >= n.len() && h[..n.len()].eq_ignore_ascii_case(n)
}

pub struct SnippetPicker {
    /// Caller-supplied rows. Must arrive sorted by key (we
    /// `debug_assert!` it in `new` so a mis-sorted test input
    /// fails loudly instead of silently rendering wrong).
    rows: Vec<PickerRow>,
    /// Current filter string. Driven by the input field.
    filter: String,
    /// Cursor index into `visible_indices`. `None` when the
    /// visible set is empty — avoids the "cursor=0 but no row
    /// at that slot" pseudo-valid state.
    cursor: Option<usize>,
    /// Indices into `rows` that match the current filter, in
    /// display order. Recomputed on every keystroke; cheap.
    visible_indices: Vec<usize>,
}

impl SnippetPicker {
    pub fn new(rows: Vec<PickerRow>, initial_filter: String) -> Self {
        debug_assert!(
            rows.windows(2).all(|w| w[0].key <= w[1].key),
            "SnippetPicker::new expects rows pre-sorted by key (Snippets::all walks a BTreeMap)",
        );
        let mut picker = Self {
            rows,
            filter: initial_filter,
            cursor: None,
            visible_indices: Vec::new(),
        };
        picker.refilter();
        picker
    }

    /// Recompute `visible_indices` from `filter`, place the cursor
    /// on the first visible row (or `None` if empty). Prefix match
    /// on the snippet key, case-insensitive. Description is shown
    /// but not searched — a one-char query would otherwise match
    /// almost every snippet through some stray letter in a
    /// description, drowning out the key-prefix hits the user is
    /// actually after.
    fn refilter(&mut self) {
        let q = self.filter.trim();
        self.visible_indices = if q.is_empty() {
            (0..self.rows.len()).collect()
        } else {
            self.rows
                .iter()
                .enumerate()
                .filter_map(|(i, r)| starts_with_icase(&r.key, q).then_some(i))
                .collect()
        };
        self.cursor = (!self.visible_indices.is_empty()).then_some(0);
    }

    /// `Some(idx)` when the picker should auto-submit: the typed
    /// filter exactly equals a snippet key AND that snippet is the
    /// sole visible match. Anything else (multiple matches, partial
    /// matches, no exact key hit) → user must press Enter.
    fn auto_submit_index(&self) -> Option<usize> {
        let [idx] = self.visible_indices[..] else {
            return None;
        };
        let key = &self.rows[idx].key;
        key.eq_ignore_ascii_case(self.filter.trim()).then_some(idx)
    }

    /// Pure key handler. Method (not free function) so the API
    /// reads naturally; tests can still drive it without spinning
    /// up a tuirealm `Application` by constructing a `SnippetPicker`
    /// directly and calling `on_key`.
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
                self.auto_submit_index()
                    .map(|idx| Msg::ChoicePicked(vec![idx]))
            }
            _ => None,
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

        let mut body: Vec<Line> = Vec::with_capacity(self.visible_indices.len().max(1));
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
                let is_cursor = self.cursor == Some(i);
                let caret = if is_cursor { "▸ " } else { "  " };
                let mut row_style = Style::default().fg(theme.text_strong);
                if is_cursor {
                    row_style = row_style.bg(theme.fill).add_modifier(Modifier::BOLD);
                }
                let origin_tag = r.origin.label();
                let mut spans: Vec<Span<'static>> = vec![
                    Span::styled(caret.to_string(), row_style),
                    Span::styled(format!("]{:<6}  ", r.key), row_style.fg(theme.accent)),
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
        match ev {
            Event::Keyboard(key) => self.on_key(key),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pilot_config::{Snippet, SnippetOrigin};

    /// Build a sorted row vec. The production caller (`mount_snippet_picker`)
    /// hands the picker rows derived from `Snippets::all()`, which walks
    /// a BTreeMap and is therefore already sorted; tests have to match
    /// that contract or trip the `debug_assert!` in `SnippetPicker::new`.
    fn make_rows() -> Vec<PickerRow> {
        // Keys: deploy, pr, rev — already alphabetical.
        vec![
            PickerRow::new(
                "deploy",
                &Snippet {
                    description: "Pre-deploy check".into(),
                    body: "deploy check".into(),
                    origin: SnippetOrigin::Repo,
                },
            ),
            PickerRow::new(
                "pr",
                &Snippet {
                    description: "Open PR".into(),
                    body: "open pr please".into(),
                    origin: SnippetOrigin::Global,
                },
            ),
            PickerRow::new(
                "rev",
                &Snippet {
                    description: "Review diff".into(),
                    body: "review please".into(),
                    origin: SnippetOrigin::Global,
                },
            ),
        ]
    }

    fn ke(c: char) -> KeyEvent {
        KeyEvent::new(Key::Char(c), KeyModifiers::NONE)
    }

    fn key(code: Key) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
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
        assert_eq!(picker.cursor, Some(0));
    }

    #[test]
    fn typing_filters_and_recovers_on_backspace() {
        let mut picker = SnippetPicker::new(make_rows(), String::new());
        let out = picker.on_key(&ke('r'));
        assert!(out.is_none(), "no auto-submit on incomplete key");
        assert_eq!(picker.visible_indices.len(), 1);
        assert_eq!(picker.rows[picker.visible_indices[0]].key, "rev");

        let _ = picker.on_key(&key(Key::Backspace));
        assert_eq!(picker.filter, "");
        assert_eq!(picker.visible_indices.len(), 3);
    }

    /// Headline test: typing a full snippet key auto-submits.
    /// `]rev` (where `r`, `e`, `v` are streamed in) should land
    /// on `ChoicePicked` after the `v` — no Enter.
    #[test]
    fn typing_full_key_auto_submits() {
        let mut picker = SnippetPicker::new(make_rows(), String::new());
        assert!(picker.on_key(&ke('r')).is_none());
        assert!(picker.on_key(&ke('e')).is_none());
        let out = picker.on_key(&ke('v'));
        match out {
            Some(Msg::ChoicePicked(v)) => {
                assert_eq!(v.len(), 1);
                assert_eq!(picker.rows[v[0]].key, "rev");
            }
            other => panic!("expected ChoicePicked, got {other:?}"),
        }
    }

    /// Mounted with an initial filter (the `]r` flow).
    #[test]
    fn initial_filter_carries_through() {
        let mut picker = SnippetPicker::new(make_rows(), "r".into());
        assert_eq!(picker.visible_indices.len(), 1);
        assert!(picker.on_key(&ke('e')).is_none());
        match picker.on_key(&ke('v')) {
            Some(Msg::ChoicePicked(v)) => assert_eq!(picker.rows[v[0]].key, "rev"),
            other => panic!("expected ChoicePicked, got {other:?}"),
        }
    }

    /// Enter on the cursor submits even when the filter prefixes
    /// multiple keys (the discovery / manual-pick path).
    #[test]
    fn enter_submits_cursor_selection() {
        // Pre-sorted: ping, pr, rev. Both ping and pr start with `p`.
        let rows = vec![
            PickerRow::new(
                "ping",
                &Snippet {
                    description: "Ping".into(),
                    body: "ping body".into(),
                    origin: SnippetOrigin::Global,
                },
            ),
            PickerRow::new(
                "pr",
                &Snippet {
                    description: "Open PR".into(),
                    body: "pr body".into(),
                    origin: SnippetOrigin::Global,
                },
            ),
            PickerRow::new(
                "rev",
                &Snippet {
                    description: "Review diff".into(),
                    body: "review body".into(),
                    origin: SnippetOrigin::Global,
                },
            ),
        ];
        let mut picker = SnippetPicker::new(rows, String::new());
        assert!(picker.on_key(&ke('p')).is_none());
        assert_eq!(picker.visible_indices.len(), 2);
        let _ = picker.on_key(&key(Key::Down));
        match picker.on_key(&key(Key::Enter)) {
            Some(Msg::ChoicePicked(v)) => assert_eq!(picker.rows[v[0]].key, "pr"),
            other => panic!("expected ChoicePicked, got {other:?}"),
        }
    }

    #[test]
    fn esc_dismisses() {
        let mut picker = SnippetPicker::new(make_rows(), String::new());
        let out = picker.on_key(&key(Key::Esc));
        assert!(matches!(out, Some(Msg::ModalDismissed)));
    }

    /// `ctrl-c` also dismisses (terminal-pane users keep that as
    /// their muscle-memory cancel).
    #[test]
    fn ctrl_c_dismisses() {
        let mut picker = SnippetPicker::new(make_rows(), String::new());
        let out = picker.on_key(&KeyEvent::new(Key::Char('c'), KeyModifiers::CONTROL));
        assert!(matches!(out, Some(Msg::ModalDismissed)));
    }

    #[test]
    fn picker_with_no_snippets_handles_typing_safely() {
        let mut picker = SnippetPicker::new(vec![], String::new());
        assert!(picker.cursor.is_none());
        assert!(picker.on_key(&ke('x')).is_none());
        assert!(picker.visible_indices.is_empty());
        // Enter on empty visible set is a no-op (no row to submit).
        assert!(picker.on_key(&key(Key::Enter)).is_none());
    }

    /// Filter narrowing to zero matches clears the cursor — Enter
    /// then no longer panics on out-of-bounds index access.
    #[test]
    fn no_matches_clears_cursor_and_enter_is_noop() {
        let mut picker = SnippetPicker::new(make_rows(), "zzz".into());
        assert!(picker.visible_indices.is_empty());
        assert!(picker.cursor.is_none());
        assert!(picker.on_key(&key(Key::Enter)).is_none());
    }

    /// Filter is case-insensitive — typing `REV` against the `rev`
    /// snippet should still auto-submit.
    #[test]
    fn filter_is_case_insensitive() {
        let mut picker = SnippetPicker::new(make_rows(), String::new());
        for c in ['R', 'E', 'V'] {
            let out = picker.on_key(&ke(c));
            if let Some(Msg::ChoicePicked(v)) = out {
                assert_eq!(picker.rows[v[0]].key, "rev");
                return;
            }
        }
        panic!("expected auto-submit on case-insensitive full key");
    }
}
