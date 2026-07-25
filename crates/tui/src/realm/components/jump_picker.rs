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

    pub fn on_key(&mut self, key: &KeyEvent) -> Option<Msg> {
        self.dispatch_key(key)
    }
}

impl FilterableList for JumpPicker {
    fn compute_visible(&mut self) -> Vec<usize> {
        let q = self.filter.trim();
        if q.is_empty() {
            (0..self.labels.len()).collect()
        } else {
            self.labels
                .iter()
                .enumerate()
                .filter_map(|(i, l)| subsequence_icase(l, q).then_some(i))
                .collect()
        }
    }

    fn pick(&self, item_idx: usize) -> Option<Msg> {
        let key = self.keys.get(item_idx)?.clone();
        Some(Msg::ChoicePicked(vec![ChoicePayload::Session(key)]))
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

impl Component for JumpPicker {
    fn view(&mut self, frame: &mut Frame, area: Rect) {
        let theme = crate::theme::current();
        let empty = if self.labels.is_empty() {
            "  (no workspaces to jump to)"
        } else {
            "  (no matches)"
        };
        let help = vec![
            Span::styled("↑↓", Style::default().fg(theme.accent).bold()),
            Span::raw(" navigate  "),
            Span::styled("Enter", Style::default().fg(theme.success).bold()),
            Span::raw(" jump  "),
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
                title: " Jump to workspace ",
                modal_w: 80,
                empty,
                help,
            },
            |row_idx, is_cursor| {
                let caret = if is_cursor { "▸ " } else { "  " };
                let mut row_style = Style::default().fg(theme.text_strong);
                if is_cursor {
                    row_style = row_style.bg(theme.fill).add_modifier(Modifier::BOLD);
                }
                Line::from(vec![
                    Span::styled(caret.to_string(), row_style),
                    Span::styled(self.labels[row_idx].clone(), row_style),
                ])
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
