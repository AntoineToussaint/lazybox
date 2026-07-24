//! `JumpPicker` — fuzzy switcher over every workspace (issue #171).
//!
//! The general "jump to workspace" the narrow `!` / `Shift-F` jumps
//! lacked: a single picker spanning all repos. Typing fuzzy-filters
//! the rows (a case-insensitive subsequence match over the label, so
//! `lbx171` finds `lazybox#171`); ↑/↓ navigate; Enter lands the
//! cursor on the chosen workspace via `focus_workspace_key`.
//!
//! Modal returns:
//! - `Msg::ChoicePicked(vec![ChoicePayload::Session(key)])` — the
//!   chosen workspace's session key, carried on the row itself so a
//!   filtered/re-ordered display can't resolve to the wrong workspace
//!   (issue #512). No parallel model-side stash.
//! - `Msg::ModalDismissed` — Esc or Ctrl-C.
//!
//! Unlike the snippet picker this never auto-submits: a workspace
//! label rarely equals the filter exactly, and a stray auto-jump
//! mid-type would be jarring. The user always confirms with Enter.

use crate::realm::ChoicePayload;
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

/// Case-insensitive subsequence test: do all chars of `needle` appear
/// in `haystack`, in order (gaps allowed)? Allocation-free — the
/// filter re-runs on every keystroke.
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

pub struct JumpPicker {
    /// Display labels, index-aligned with [`Self::keys`]. A picked row
    /// maps straight to its key — the two Vecs are built together and
    /// never re-ordered, so the filtered display can't desync them.
    labels: Vec<String>,
    /// Session key for each label (same length / order). Reported as
    /// the pick's [`ChoicePayload::Session`].
    keys: Vec<lazybox_core::SessionKey>,
    /// Current filter string.
    filter: String,
    /// Cursor index into `visible_indices`. `None` when empty.
    cursor: Option<usize>,
    /// Indices into `labels` matching the filter, in display order.
    visible_indices: Vec<usize>,
}

impl JumpPicker {
    /// `rows` pairs each display label with the workspace session key
    /// it jumps to. The pairing travels through the picker so Enter
    /// always resolves to the key of the row the user highlighted.
    pub fn new(rows: Vec<(String, lazybox_core::SessionKey)>) -> Self {
        let (labels, keys): (Vec<_>, Vec<_>) = rows.into_iter().unzip();
        let mut picker = Self {
            labels,
            keys,
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
            (0..self.labels.len()).collect()
        } else {
            self.labels
                .iter()
                .enumerate()
                .filter_map(|(i, l)| subsequence_icase(l, q).then_some(i))
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
                let key = self.keys.get(row_idx)?.clone();
                Some(Msg::ChoicePicked(vec![ChoicePayload::Session(key)]))
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

impl Component for JumpPicker {
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
            .title(Span::styled(" Jump to workspace ", theme.modal_title()))
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
                if self.labels.is_empty() {
                    "  (no workspaces to jump to)"
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
                let is_cursor = self.cursor == Some(i);
                let caret = if is_cursor { "▸ " } else { "  " };
                let mut row_style = Style::default().fg(theme.text_strong);
                if is_cursor {
                    row_style = row_style.bg(theme.fill).add_modifier(Modifier::BOLD);
                }
                body.push(Line::from(vec![
                    Span::styled(caret.to_string(), row_style),
                    Span::styled(self.labels[row_idx].clone(), row_style),
                ]));
            }
        }
        frame.render_widget(Paragraph::new(body), body_rect);

        let help_spans = vec![
            Span::styled("↑↓", Style::default().fg(theme.accent).bold()),
            Span::raw(" navigate  "),
            Span::styled("Enter", Style::default().fg(theme.success).bold()),
            Span::raw(" jump  "),
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

impl AppComponent<Msg, UserEvent> for JumpPicker {
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
    fn labels() -> Vec<(String, lazybox_core::SessionKey)> {
        vec![
            ("owner/repo#10  Fix the parser".to_string(), key_at(0)),
            ("owner/repo#22  Add jump picker".to_string(), key_at(1)),
            ("other/proj#3  Cleanup".to_string(), key_at(2)),
        ]
    }
    fn key_at(i: usize) -> lazybox_core::SessionKey {
        lazybox_core::SessionKey::new(format!("ws-{i}"))
    }

    #[test]
    fn empty_filter_shows_all_rows() {
        let p = JumpPicker::new(labels());
        assert_eq!(p.visible_indices, vec![0, 1, 2]);
        assert_eq!(p.cursor, Some(0));
    }

    #[test]
    fn subsequence_filter_matches_out_of_order_gaps() {
        let mut p = JumpPicker::new(labels());
        // `jpk` is a subsequence of "Add jump picker" (j-ump p-i-c-k-er)
        // with gaps, but not of the other rows.
        for c in ['j', 'p', 'k'] {
            assert!(p.on_key(&ke(c)).is_none());
        }
        assert_eq!(p.visible_indices, vec![1]);
        assert_eq!(
            p.labels[p.visible_indices[0]],
            "owner/repo#22  Add jump picker"
        );
    }

    #[test]
    fn filter_is_case_insensitive() {
        let mut p = JumpPicker::new(labels());
        for c in ['C', 'L', 'E'] {
            let _ = p.on_key(&ke(c));
        }
        // "Cleanup" matches CLE.
        assert_eq!(p.visible_indices, vec![2]);
    }

    #[test]
    fn typing_never_auto_submits() {
        let mut p = JumpPicker::new(labels());
        // Even when the filter narrows to a single row, no submit
        // without Enter — distinct from the snippet picker.
        for c in "Add jump picker".chars() {
            assert!(p.on_key(&ke(c)).is_none());
        }
        assert_eq!(p.visible_indices, vec![1]);
    }

    #[test]
    fn enter_submits_the_cursor_row() {
        let mut p = JumpPicker::new(labels());
        let _ = p.on_key(&key(Key::Down));
        match p.on_key(&key(Key::Enter)) {
            // Cursor on row 1 → that row's session key, not its index.
            Some(Msg::ChoicePicked(v)) => {
                assert_eq!(v, vec![ChoicePayload::Session(key_at(1))]);
            }
            other => panic!("expected ChoicePicked, got {other:?}"),
        }
    }

    #[test]
    fn backspace_widens_the_filter() {
        let mut p = JumpPicker::new(labels());
        let _ = p.on_key(&ke('z'));
        assert!(p.visible_indices.is_empty());
        assert!(p.cursor.is_none());
        // Enter with no match is a no-op (no panic).
        assert!(p.on_key(&key(Key::Enter)).is_none());
        let _ = p.on_key(&key(Key::Backspace));
        assert_eq!(p.visible_indices, vec![0, 1, 2]);
    }

    #[test]
    fn esc_and_ctrl_c_dismiss() {
        let mut p = JumpPicker::new(labels());
        assert!(matches!(
            p.on_key(&key(Key::Esc)),
            Some(Msg::ModalDismissed)
        ));
        let mut p = JumpPicker::new(labels());
        let ev = KeyEvent::new(Key::Char('c'), KeyModifiers::CONTROL);
        assert!(matches!(p.on_key(&ev), Some(Msg::ModalDismissed)));
    }

    #[test]
    fn filtered_display_resolves_to_the_right_key_not_the_visible_row() {
        // Regression for #512: filtering collapses the visible list to
        // a single row at *visible* position 0, but that row's real
        // identity is item index 1. The pick must carry row 1's session
        // key — a positional resolver would have grabbed key_at(0).
        let mut p = JumpPicker::new(labels());
        for c in ['j', 'p', 'k'] {
            let _ = p.on_key(&ke(c));
        }
        assert_eq!(p.visible_indices, vec![1], "only the jump-picker row");
        assert_eq!(p.cursor, Some(0), "cursor sits at visible position 0");
        match p.on_key(&key(Key::Enter)) {
            Some(Msg::ChoicePicked(v)) => {
                assert_eq!(v, vec![ChoicePayload::Session(key_at(1))]);
            }
            other => panic!("expected ChoicePicked, got {other:?}"),
        }
    }

    #[test]
    fn empty_picker_is_safe() {
        let mut p = JumpPicker::new(vec![]);
        assert!(p.cursor.is_none());
        assert!(p.on_key(&ke('x')).is_none());
        assert!(p.on_key(&key(Key::Enter)).is_none());
    }
}
