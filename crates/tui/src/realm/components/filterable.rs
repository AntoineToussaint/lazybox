//! `FilterableList` — the shared core behind the filter-as-you-type
//! pickers (`jump_picker`, `prompt_history_picker`, `snippet_picker`).
//!
//! Those three grew as copies of one another and drifted: near-identical
//! `refilter`, cursor movement, viewport windowing, and the Esc / Ctrl-c
//! / Up / Down / Home / End / Enter / Backspace / char key protocol were
//! re-implemented per picker, so a key-handling fix had to be applied two
//! or three times (issue #549). This module owns that protocol once.
//!
//! A picker keeps its own item store and the three state fields the
//! protocol mutates (`filter`, `cursor`, `visible_indices` — named so the
//! existing in-module tests keep reaching them directly) and implements
//! [`FilterableList`] to expose them plus the two things that genuinely
//! differ per picker: how the filter maps to a visible index list
//! ([`FilterableList::compute_visible`]) and how a picked row becomes a
//! [`Msg`] ([`FilterableList::pick`]). Two opt-in hooks cover the rest of
//! the real divergence:
//!
//! - [`FilterableList::auto_submit`] — the snippet picker submits the
//!   instant the typed filter uniquely identifies a key (`]]srev`); the
//!   jump and prompt-history pickers never auto-submit and leave it at
//!   the default.
//! - [`FilterableList::custom_key`] — keys outside the common protocol
//!   (the snippet picker's `Ctrl-F` "free text" escape).
//!
//! [`render_filter_modal`] draws the shared single-column chrome (centered
//! rounded block, filter input line, divider, cursor-tracking viewport,
//! help line) for the two simple pickers; the snippet picker keeps its
//! bespoke two-pane / category-header layout and only shares the state +
//! key protocol.

use crate::realm::Msg;
use tuirealm::event::{Key, KeyEvent, KeyModifiers};
use tuirealm::ratatui::Frame;
use tuirealm::ratatui::layout::Rect;
use tuirealm::ratatui::prelude::*;
use tuirealm::ratatui::widgets::{Block, BorderType, Borders, Clear, Paragraph};

/// Case-insensitive subsequence test: do all chars of `needle` appear in
/// `haystack`, in order (gaps allowed)? Allocation-free — the filter
/// re-runs on every keystroke. Shared by the jump and prompt-history
/// pickers (the snippet picker matches by substring instead).
pub fn subsequence_icase(haystack: &str, needle: &str) -> bool {
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

/// The contract every filter-as-you-type picker implements. The provided
/// [`FilterableList::dispatch_key`] is the single home of the common key
/// protocol; everything above it is per-picker data the protocol needs.
pub trait FilterableList {
    /// Recompute the visible item indices (into the picker's own store),
    /// in display order, for the current [`FilterableList::filter`]. Takes
    /// `&mut self` so a picker can refresh grouping state it derives
    /// alongside the list (the snippet picker's "Recent" group + list
    /// scroll). The cursor is reset by [`FilterableList::refilter`].
    fn compute_visible(&mut self) -> Vec<usize>;

    /// Resolve a picked underlying item index to its result message.
    /// `item_idx` is an index into the picker's store, already mapped
    /// back from the visible cursor position — so a filtered / re-ordered
    /// display can't submit the wrong row (issue #512).
    fn pick(&self, item_idx: usize) -> Option<Msg>;

    /// Resolve a `Shift-Enter` pick: insert the row *without* submitting
    /// it, so the user can edit before sending (issue #791). Only the
    /// snippet picker distinguishes the two; the default is a plain
    /// [`FilterableList::pick`], so every other picker treats `Shift-Enter`
    /// exactly like `Enter`.
    fn pick_no_submit(&self, item_idx: usize) -> Option<Msg> {
        self.pick(item_idx)
    }

    // ── state accessors (the picker owns the fields) ──────────────────
    fn filter(&self) -> &str;
    fn filter_mut(&mut self) -> &mut String;
    fn cursor(&self) -> Option<usize>;
    fn set_cursor(&mut self, cursor: Option<usize>);
    fn visible(&self) -> &[usize];
    fn set_visible(&mut self, visible: Vec<usize>);

    // ── hooks (opt-in per picker) ─────────────────────────────────────
    /// After a character is typed and the list refilters, a picker may
    /// auto-submit (the snippet picker's unique-prefix fast path). The
    /// default never fires — typing only filters.
    fn auto_submit(&self) -> Option<Msg> {
        None
    }

    /// Handle a key outside the common protocol (the snippet picker's
    /// `Ctrl-F` free-text escape). Returns `Some` to consume it.
    fn custom_key(&mut self, _key: &KeyEvent) -> Option<Msg> {
        None
    }

    // ── provided: the shared protocol ─────────────────────────────────
    /// Recompute the visible list and snap the cursor to the first row
    /// (or `None` when nothing matches). Call after any filter edit and
    /// once at construction.
    fn refilter(&mut self) {
        let visible = self.compute_visible();
        let cursor = (!visible.is_empty()).then_some(0);
        self.set_visible(visible);
        self.set_cursor(cursor);
    }

    /// The underlying item index under the cursor, if any.
    fn selected(&self) -> Option<usize> {
        self.cursor().and_then(|c| self.visible().get(c).copied())
    }

    /// Apply one keypress. This is the whole common protocol — Esc /
    /// Ctrl-c dismiss, Up / Down / Home / End move the cursor, Enter
    /// picks, Backspace / char edit the filter (and may auto-submit) —
    /// so a change here reaches every picker at once.
    fn dispatch_key(&mut self, key: &KeyEvent) -> Option<Msg> {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        if matches!(key.code, Key::Esc) || (ctrl && matches!(key.code, Key::Char('c'))) {
            return Some(Msg::ModalDismissed);
        }
        match key.code {
            Key::Down => {
                if let Some(c) = self.cursor()
                    && c + 1 < self.visible().len()
                {
                    self.set_cursor(Some(c + 1));
                }
                None
            }
            Key::Up => {
                if let Some(c) = self.cursor()
                    && c > 0
                {
                    self.set_cursor(Some(c - 1));
                }
                None
            }
            Key::Home => {
                if !self.visible().is_empty() {
                    self.set_cursor(Some(0));
                }
                None
            }
            Key::End => {
                if !self.visible().is_empty() {
                    self.set_cursor(Some(self.visible().len() - 1));
                }
                None
            }
            // Shift-Enter commits the highlighted row without submitting
            // (the snippet picker inserts the body into the composer for
            // editing, #791); plain Enter submits. The terminal reports
            // Shift-Enter distinctly under the pushed keyboard-enhancement
            // flags, the same signal the terminal composer reads.
            Key::Enter if key.modifiers.contains(KeyModifiers::SHIFT) => {
                self.selected().and_then(|idx| self.pick_no_submit(idx))
            }
            Key::Enter => self.selected().and_then(|idx| self.pick(idx)),
            Key::Backspace => {
                self.filter_mut().pop();
                self.refilter();
                None
            }
            Key::Char(c) if !ctrl => {
                self.filter_mut().push(c);
                self.refilter();
                self.auto_submit()
            }
            _ => self.custom_key(key),
        }
    }
}

/// The per-picker static chrome for [`render_filter_modal`]: the modal
/// title, its target width, the empty-state message, and the help line.
pub struct FilterModalChrome<'a> {
    pub title: &'a str,
    pub modal_w: u16,
    pub empty: &'a str,
    pub help: Vec<Span<'static>>,
}

/// Draw the shared single-column picker chrome: a centered rounded modal
/// with a filter-input line, a divider, a cursor-tracking viewport, and a
/// help line. `render_row` builds one body line for `(item_idx,
/// is_cursor)`, so each picker keeps its own row styling while the frame,
/// filter line, windowing, and empty-state are shared.
pub fn render_filter_modal<P: FilterableList>(
    picker: &P,
    frame: &mut Frame,
    area: Rect,
    theme: &crate::theme::Theme,
    chrome: FilterModalChrome<'_>,
    mut render_row: impl FnMut(usize, bool) -> Line<'static>,
) {
    let FilterModalChrome {
        title,
        modal_w,
        empty,
        help,
    } = chrome;
    let modal_w = modal_w.min(area.width.saturating_sub(4));
    let modal_h = 24u16.min(area.height.saturating_sub(4));
    let x = area.x + area.width.saturating_sub(modal_w) / 2;
    let y = area.y + area.height.saturating_sub(modal_h) / 2;
    let modal = Rect::new(x, y, modal_w, modal_h);

    frame.render_widget(Clear, modal);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .title(Span::styled(title.to_string(), theme.modal_title()))
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
        Span::styled(
            picker.filter().to_string(),
            Style::default().fg(theme.text_strong),
        ),
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
    let cursor = picker.cursor().unwrap_or(0);
    let start = cursor.saturating_sub(rows.saturating_sub(1));
    let mut body: Vec<Line> = Vec::with_capacity(rows.max(1));
    if picker.visible().is_empty() {
        body.push(Line::from(Span::styled(
            empty,
            Style::default().fg(theme.text_dim).italic(),
        )));
    } else {
        for (i, &row_idx) in picker.visible().iter().enumerate().skip(start).take(rows) {
            body.push(render_row(row_idx, picker.cursor() == Some(i)));
        }
    }
    frame.render_widget(Paragraph::new(body), body_rect);

    frame.render_widget(Paragraph::new(Line::from(help)), help_rect);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::realm::ChoicePayload;

    fn ke(c: char) -> KeyEvent {
        KeyEvent::new(Key::Char(c), KeyModifiers::NONE)
    }
    fn key(code: Key) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    /// A minimal picker over string labels, matched by substring, used to
    /// exercise the protocol without pulling in a real picker's data.
    struct Fake {
        labels: Vec<String>,
        filter: String,
        cursor: Option<usize>,
        visible_indices: Vec<usize>,
        auto: bool,
    }

    impl Fake {
        fn new(labels: &[&str]) -> Self {
            let mut f = Self {
                labels: labels.iter().map(|s| s.to_string()).collect(),
                filter: String::new(),
                cursor: None,
                visible_indices: Vec::new(),
                auto: false,
            };
            f.refilter();
            f
        }
        fn with_auto(mut self) -> Self {
            self.auto = true;
            self
        }
    }

    impl FilterableList for Fake {
        fn compute_visible(&mut self) -> Vec<usize> {
            let q = self.filter.trim().to_ascii_lowercase();
            (0..self.labels.len())
                .filter(|&i| q.is_empty() || self.labels[i].to_ascii_lowercase().contains(&q))
                .collect()
        }
        fn pick(&self, item_idx: usize) -> Option<Msg> {
            self.labels
                .get(item_idx)
                .map(|l| Msg::ChoicePicked(vec![ChoicePayload::Text(l.clone())]))
        }
        // Route Shift-Enter to the no-submit variant so the protocol test
        // can tell the two commit keys apart.
        fn pick_no_submit(&self, item_idx: usize) -> Option<Msg> {
            self.labels
                .get(item_idx)
                .map(|l| Msg::ChoicePickedNoSubmit(vec![ChoicePayload::Text(l.clone())]))
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
        // Auto-submit when the filter narrows to exactly one row.
        fn auto_submit(&self) -> Option<Msg> {
            if self.auto && self.visible_indices.len() == 1 {
                self.pick(self.visible_indices[0])
            } else {
                None
            }
        }
    }

    #[test]
    fn empty_filter_shows_all_and_cursor_at_top() {
        let f = Fake::new(&["alpha", "beta", "gamma"]);
        assert_eq!(f.visible_indices, vec![0, 1, 2]);
        assert_eq!(f.cursor, Some(0));
    }

    #[test]
    fn typing_filters_and_backspace_widens() {
        let mut f = Fake::new(&["alpha", "beta", "gamma"]);
        assert!(f.dispatch_key(&ke('a')).is_none());
        // "alpha", "beta", "gamma" all contain 'a'.
        assert_eq!(f.visible_indices, vec![0, 1, 2]);
        assert!(f.dispatch_key(&ke('l')).is_none());
        // only "alpha" contains "al".
        assert_eq!(f.visible_indices, vec![0]);
        assert!(f.dispatch_key(&key(Key::Backspace)).is_none());
        assert_eq!(f.visible_indices, vec![0, 1, 2]);
    }

    #[test]
    fn cursor_moves_and_clamps_at_both_ends() {
        let mut f = Fake::new(&["a", "b", "c"]);
        assert_eq!(f.cursor, Some(0));
        // Up at the top is a no-op.
        let _ = f.dispatch_key(&key(Key::Up));
        assert_eq!(f.cursor, Some(0));
        let _ = f.dispatch_key(&key(Key::Down));
        let _ = f.dispatch_key(&key(Key::Down));
        assert_eq!(f.cursor, Some(2));
        // Down at the bottom is a no-op.
        let _ = f.dispatch_key(&key(Key::Down));
        assert_eq!(f.cursor, Some(2));
        let _ = f.dispatch_key(&key(Key::Home));
        assert_eq!(f.cursor, Some(0));
        let _ = f.dispatch_key(&key(Key::End));
        assert_eq!(f.cursor, Some(2));
    }

    #[test]
    fn enter_picks_the_cursor_row_by_item_index() {
        let mut f = Fake::new(&["alpha", "beta", "gamma"]);
        // Filter to a single row whose *item* index is 2.
        for c in "gamma".chars() {
            let _ = f.dispatch_key(&ke(c));
        }
        assert_eq!(f.visible_indices, vec![2]);
        assert_eq!(f.cursor, Some(0), "cursor sits at visible position 0");
        match f.dispatch_key(&key(Key::Enter)) {
            Some(Msg::ChoicePicked(v)) => {
                assert_eq!(v, vec![ChoicePayload::Text("gamma".into())])
            }
            other => panic!("expected pick of item 2, got {other:?}"),
        }
    }

    /// Shift-Enter routes through `pick_no_submit` while plain Enter goes
    /// through `pick` — the two commit keys the snippet picker distinguishes
    /// (issue #791).
    #[test]
    fn shift_enter_routes_to_pick_no_submit() {
        let mut f = Fake::new(&["alpha", "beta"]);
        let shift_enter = KeyEvent::new(Key::Enter, KeyModifiers::SHIFT);
        match f.dispatch_key(&shift_enter) {
            Some(Msg::ChoicePickedNoSubmit(v)) => {
                assert_eq!(v, vec![ChoicePayload::Text("alpha".into())])
            }
            other => panic!("Shift-Enter must pick without submit, got {other:?}"),
        }
        // Plain Enter still submits.
        match f.dispatch_key(&key(Key::Enter)) {
            Some(Msg::ChoicePicked(v)) => {
                assert_eq!(v, vec![ChoicePayload::Text("alpha".into())])
            }
            other => panic!("Enter must submit, got {other:?}"),
        }
    }

    /// The default `pick_no_submit` falls back to `pick`, so a picker that
    /// doesn't override it treats Shift-Enter exactly like Enter.
    #[test]
    fn pick_no_submit_defaults_to_pick() {
        struct Plain(Vec<String>, Option<usize>, Vec<usize>, String);
        impl FilterableList for Plain {
            fn compute_visible(&mut self) -> Vec<usize> {
                (0..self.0.len()).collect()
            }
            fn pick(&self, i: usize) -> Option<Msg> {
                self.0
                    .get(i)
                    .map(|l| Msg::ChoicePicked(vec![ChoicePayload::Text(l.clone())]))
            }
            fn filter(&self) -> &str {
                &self.3
            }
            fn filter_mut(&mut self) -> &mut String {
                &mut self.3
            }
            fn cursor(&self) -> Option<usize> {
                self.1
            }
            fn set_cursor(&mut self, c: Option<usize>) {
                self.1 = c;
            }
            fn visible(&self) -> &[usize] {
                &self.2
            }
            fn set_visible(&mut self, v: Vec<usize>) {
                self.2 = v;
            }
        }
        let mut p = Plain(vec!["only".into()], None, Vec::new(), String::new());
        p.refilter();
        let shift_enter = KeyEvent::new(Key::Enter, KeyModifiers::SHIFT);
        assert!(
            matches!(p.dispatch_key(&shift_enter), Some(Msg::ChoicePicked(_))),
            "no override → Shift-Enter behaves like Enter",
        );
    }

    #[test]
    fn esc_and_ctrl_c_dismiss() {
        let mut f = Fake::new(&["a"]);
        assert!(matches!(
            f.dispatch_key(&key(Key::Esc)),
            Some(Msg::ModalDismissed)
        ));
        let ctrl_c = KeyEvent::new(Key::Char('c'), KeyModifiers::CONTROL);
        assert!(matches!(f.dispatch_key(&ctrl_c), Some(Msg::ModalDismissed)));
    }

    #[test]
    fn no_match_clears_cursor_and_enter_is_a_noop() {
        let mut f = Fake::new(&["alpha"]);
        let _ = f.dispatch_key(&ke('z'));
        assert!(f.visible_indices.is_empty());
        assert!(f.cursor.is_none());
        assert!(f.dispatch_key(&key(Key::Enter)).is_none());
    }

    #[test]
    fn empty_picker_is_safe() {
        let mut f = Fake::new(&[]);
        assert!(f.cursor.is_none());
        assert!(f.dispatch_key(&ke('x')).is_none());
        assert!(f.dispatch_key(&key(Key::Enter)).is_none());
    }

    #[test]
    fn auto_submit_hook_fires_only_when_opted_in() {
        // Without the hook, narrowing to one row never submits on type.
        let mut plain = Fake::new(&["alpha", "beta"]);
        assert!(plain.dispatch_key(&ke('l')).is_none());
        assert_eq!(plain.visible_indices, vec![0]);

        // With it, the keystroke that narrows to a single row submits.
        let mut auto = Fake::new(&["alpha", "beta"]).with_auto();
        match auto.dispatch_key(&ke('l')) {
            Some(Msg::ChoicePicked(v)) => {
                assert_eq!(v, vec![ChoicePayload::Text("alpha".into())])
            }
            other => panic!("expected auto-submit, got {other:?}"),
        }
    }

    #[test]
    fn backspace_never_auto_submits() {
        // Deleting is not "type to submit": even an auto-submit picker
        // that lands on a single row via Backspace must stay open.
        let mut auto = Fake::new(&["alpha", "alto"]).with_auto();
        for c in "alt".chars() {
            let _ = auto.dispatch_key(&ke(c));
        }
        // "alt" matches only "alto" — but it auto-submitted on the way,
        // so start from a filter that keeps two rows then delete to one.
        let mut auto = Fake::new(&["alpha", "alto"]).with_auto();
        let _ = auto.dispatch_key(&ke('a')); // both
        let _ = auto.dispatch_key(&ke('l')); // both
        assert_eq!(auto.visible_indices, vec![0, 1]);
        // Append 'x' → no match, then Backspace back to two rows: the
        // Backspace itself must not fire auto-submit.
        let _ = auto.dispatch_key(&ke('x'));
        assert!(auto.visible_indices.is_empty());
        assert!(auto.dispatch_key(&key(Key::Backspace)).is_none());
        assert_eq!(auto.visible_indices, vec![0, 1]);
    }
}
