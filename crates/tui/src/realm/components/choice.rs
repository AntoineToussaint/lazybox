//! `Choice<T>` — single- or multi-select picker. tuirealm port of
//! `tui_kit::widgets::ChoiceModal`.
//!
//! Each picked row reports a typed [`ChoicePayload`] rather than a bare
//! positional index. A `.payload_for(|item| …)` closure derives the
//! payload from the *same* `T` the row displays, so the value the
//! `ChoicePicked` handler resolves always matches the row the user saw
//! — even when the caller sorts or groups the list (issue #512). When
//! no `payload_for` is set the row falls back to
//! [`ChoicePayload::Index`] (its position in `items`), which pickers
//! that resolve positionally into a component-local list still rely on.
//!
//! Modes:
//! - `Choice::single(prompt, items)` — Enter picks one, returns
//!   `ChoicePicked(vec![payload])`.
//! - `Choice::multi(prompt, items)` — Space toggles, Enter confirms,
//!   returns `ChoicePicked(vec![payload, …])`.
//!
//! `with_back(true)` enables Backspace → `Msg::ChoiceBack`.
//! `with_refresh(true)` enables `r` → `Msg::ChoiceRefresh`.

use crate::realm::ChoicePayload;
use crate::realm::Msg;
use crate::realm::UserEvent;
use tuirealm::command::{Cmd, CmdResult};
use tuirealm::component::{AppComponent, Component};
use tuirealm::event::{Event, Key, KeyModifiers};
use tuirealm::props::{AttrValue, Attribute, QueryResult};
use tuirealm::ratatui::Frame;
use tuirealm::ratatui::layout::Rect;
use tuirealm::ratatui::prelude::*;
use tuirealm::ratatui::widgets::{Block, BorderType, Borders, Clear, Paragraph};
use tuirealm::state::State;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    Single,
    Multi,
}

type LabelFn<T> = Box<dyn Fn(&T) -> String + Send>;
type SectionFn<T> = Box<dyn Fn(&T) -> &'static str + Send>;
type SelectableFn<T> = Box<dyn Fn(&T) -> bool + Send>;
type HighlightFn<T> = Box<dyn Fn(&T) + Send>;
type PayloadFn<T> = Box<dyn Fn(&T) -> ChoicePayload + Send>;

/// Single- or multi-select picker.
pub struct Choice<T: Clone + 'static + Send> {
    title: String,
    prompt: String,
    items: Vec<T>,
    selected: Vec<bool>,
    cursor: usize,
    mode: Mode,
    label_for: LabelFn<T>,
    /// Derives the typed [`ChoicePayload`] reported for a picked row
    /// from the row's own `T`. `None` falls back to
    /// [`ChoicePayload::Index`] (the row's position in `items`). Because
    /// it reads the same `T` the row displays, the payload can never
    /// drift out of step with the rendered order (issue #512).
    payload_for: Option<PayloadFn<T>>,
    can_back: bool,
    section_for: Option<SectionFn<T>>,
    selectable: Option<SelectableFn<T>>,
    can_refresh: bool,
    require_one: bool,
    show_empty_hint: bool,
    /// Fired with the newly-highlighted item every time the cursor
    /// moves (and once at mount). Side-effect only — drives live
    /// preview, e.g. the theme picker applying a palette as you arrow
    /// through. Distinct from picking: the effect is provisional until
    /// Enter confirms, and the caller restores prior state on Esc.
    on_highlight: Option<HighlightFn<T>>,
    /// Topmost visible body line. Updated lazily in `view` so the
    /// cursor stays on-screen as j/k walk past either edge of
    /// `body_area`. The list is too big for a non-scrolling modal
    /// once item count exceeds ~20 rows (the modal cap is 24 high
    /// before the prompt + help line trimming).
    scroll: u16,
    /// Last-rendered body height. Cached so PageUp/PageDown can
    /// jump a full screen at a time. Set during `view`; defaults
    /// to a reasonable fallback before the first render.
    body_height: u16,
}

impl<T: Clone + 'static + Send> Choice<T> {
    /// Single-pick mode.
    pub fn single(prompt: impl Into<String>, items: Vec<T>) -> Self {
        let len = items.len();
        Self {
            title: "Pick one".into(),
            prompt: prompt.into(),
            items,
            selected: vec![false; len],
            cursor: 0,
            mode: Mode::Single,
            label_for: Box::new(|_| String::new()),
            payload_for: None,
            can_back: false,
            section_for: None,
            selectable: None,
            can_refresh: false,
            require_one: true,
            show_empty_hint: false,
            on_highlight: None,
            scroll: 0,
            body_height: 10,
        }
    }

    /// Multi-pick mode.
    pub fn multi(prompt: impl Into<String>, items: Vec<T>) -> Self {
        let len = items.len();
        Self {
            title: "Pick any".into(),
            prompt: prompt.into(),
            items,
            selected: vec![false; len],
            cursor: 0,
            mode: Mode::Multi,
            label_for: Box::new(|_| String::new()),
            payload_for: None,
            can_back: false,
            section_for: None,
            selectable: None,
            can_refresh: false,
            require_one: true,
            show_empty_hint: false,
            on_highlight: None,
            scroll: 0,
            body_height: 10,
        }
    }

    /// Override modal title.
    pub fn title(mut self, title: impl Into<String>) -> Self {
        self.title = title.into();
        self
    }

    /// Display formatter for each item.
    pub fn label<F>(mut self, f: F) -> Self
    where
        F: Fn(&T) -> String + Send + 'static,
    {
        self.label_for = Box::new(f);
        self
    }

    /// Derive the typed [`ChoicePayload`] each picked row reports, from
    /// the row's own `T`. Set this so the `ChoicePicked` handler
    /// resolves the pick from the value that travelled *with* the
    /// displayed row instead of indexing back into a parallel Vec —
    /// which is what let a re-ordered / grouped list resolve to the
    /// wrong item (issue #512). Without it, rows report
    /// [`ChoicePayload::Index`].
    pub fn payload_for<F>(mut self, f: F) -> Self
    where
        F: Fn(&T) -> ChoicePayload + Send + 'static,
    {
        self.payload_for = Some(Box::new(f));
        self
    }

    /// Enable Backspace → `Msg::ChoiceBack`.
    pub fn with_back(mut self, enabled: bool) -> Self {
        self.can_back = enabled;
        self
    }

    /// Optional section grouper.
    pub fn section_for<F>(mut self, f: F) -> Self
    where
        F: Fn(&T) -> &'static str + Send + 'static,
    {
        self.section_for = Some(Box::new(f));
        self
    }

    /// Optional selectability predicate.
    pub fn selectable<F>(mut self, f: F) -> Self
    where
        F: Fn(&T) -> bool + Send + 'static,
    {
        self.selectable = Some(Box::new(f));
        self
    }

    /// Enable `r` → `Msg::ChoiceRefresh`.
    pub fn with_refresh(mut self, enabled: bool) -> Self {
        self.can_refresh = enabled;
        self
    }

    /// Multi-select: allow Enter on empty selection.
    pub fn allow_empty(mut self, allowed: bool) -> Self {
        self.require_one = !allowed;
        self
    }

    /// Pre-tick items matching the predicate. Used by setup steps that
    /// remount the same picker after a refresh / back navigation —
    /// keeps the user's prior selection visible.
    pub fn with_selected_by<F>(mut self, f: F) -> Self
    where
        F: Fn(&T) -> bool,
    {
        for (i, item) in self.items.iter().enumerate() {
            self.selected[i] = f(item);
        }
        self
    }

    /// Pre-tick items from a boolean mask aligned to `items`. Used by
    /// the setup renderer, which receives the selection as pure data
    /// (`Vec<bool>`) computed by the runner rather than a predicate.
    /// A mask shorter than `items` leaves the trailing rows un-ticked;
    /// extra entries are ignored.
    pub fn selected_mask(mut self, mask: Vec<bool>) -> Self {
        for (slot, want) in self.selected.iter_mut().zip(mask) {
            *slot = want;
        }
        self
    }

    /// Register a live-preview callback fired on every cursor move.
    /// Also fires once immediately for the initial cursor so the
    /// preview matches the highlighted row before the user touches a
    /// key (mounting the theme picker on the active theme previews it
    /// as a no-op; mounting on any other row applies it at once).
    pub fn on_highlight<F>(mut self, f: F) -> Self
    where
        F: Fn(&T) + Send + 'static,
    {
        self.on_highlight = Some(Box::new(f));
        self.fire_highlight();
        self
    }

    /// Start the cursor on a specific row (clamped). Used to open a
    /// picker pre-positioned on the current selection. Call before
    /// [`Self::on_highlight`] so the initial preview reads this row.
    pub fn select_index(mut self, idx: usize) -> Self {
        if !self.items.is_empty() {
            self.cursor = idx.min(self.items.len() - 1);
        }
        self
    }

    /// Invoke the highlight callback with the item under the cursor.
    fn fire_highlight(&self) {
        if let (Some(cb), Some(item)) = (self.on_highlight.as_ref(), self.items.get(self.cursor)) {
            cb(item);
        }
    }

    fn is_selectable(&self, idx: usize) -> bool {
        match self.items.get(idx) {
            None => false,
            Some(item) => self.selectable.as_ref().map(|f| f(item)).unwrap_or(true),
        }
    }

    fn move_cursor(&mut self, delta: isize) {
        if self.items.is_empty() {
            return;
        }
        let last = self.items.len() as isize - 1;
        let cur = self.cursor as isize;
        let target = (cur + delta).clamp(0, last) as usize;
        self.cursor = target;
        // After the move, if we landed on a non-selectable row,
        // hop in the same direction until we hit a selectable one
        // (or run off the edge — in which case fall back to the
        // first selectable row anywhere). Stops j/k from getting
        // stuck on inert section/header rows when those exist.
        if !self.is_selectable(self.cursor) {
            let dir: isize = if delta >= 0 { 1 } else { -1 };
            let mut i = self.cursor as isize;
            while i + dir >= 0 && i + dir <= last {
                i += dir;
                if self.is_selectable(i as usize) {
                    self.cursor = i as usize;
                    return;
                }
            }
            // No selectable in that direction — fall back to first
            // selectable anywhere.
            if let Some(idx) = (0..=last as usize).find(|i| self.is_selectable(*i)) {
                self.cursor = idx;
            }
        }
    }

    /// Snap to the first selectable item.
    fn cursor_to_first(&mut self) {
        if self.items.is_empty() {
            return;
        }
        if let Some(idx) = (0..self.items.len()).find(|i| self.is_selectable(*i)) {
            self.cursor = idx;
        }
    }

    /// Snap to the last selectable item.
    fn cursor_to_last(&mut self) {
        if self.items.is_empty() {
            return;
        }
        if let Some(idx) = (0..self.items.len()).rev().find(|i| self.is_selectable(*i)) {
            self.cursor = idx;
        }
    }

    /// Resolve the payload reported for the row at `idx`: the
    /// `payload_for` closure applied to that exact item, or the
    /// positional [`ChoicePayload::Index`] fallback.
    fn payload_at(&self, idx: usize) -> ChoicePayload {
        match (self.payload_for.as_ref(), self.items.get(idx)) {
            (Some(f), Some(item)) => f(item),
            _ => ChoicePayload::Index(idx),
        }
    }

    fn confirm_picks(&mut self) -> ConfirmResult {
        // An empty list has nothing to confirm — Enter dismisses
        // instead of latching the "pick at least one" hint, which a
        // `require_one` multi-select would otherwise do forever.
        if self.items.is_empty() {
            return ConfirmResult::Cancel;
        }
        let picked: Vec<usize> = match self.mode {
            Mode::Single => {
                if self.items.is_empty() {
                    return ConfirmResult::Cancel;
                }
                if !self.is_selectable(self.cursor) {
                    return ConfirmResult::Stay;
                }
                vec![self.cursor]
            }
            Mode::Multi => self
                .selected
                .iter()
                .enumerate()
                .filter(|(_, s)| **s)
                .map(|(i, _)| i)
                .collect(),
        };
        if self.mode == Mode::Multi && self.require_one && picked.is_empty() {
            self.show_empty_hint = true;
            return ConfirmResult::Stay;
        }
        // Map each picked row index through `payload_at` so the reported
        // value derives from the row itself, not its position.
        let payloads = picked.into_iter().map(|i| self.payload_at(i)).collect();
        ConfirmResult::Picked(payloads)
    }

    /// Returns the laid-out lines plus the line index of the cursor
    /// row (so `view` can compute a scroll offset that keeps it on
    /// screen). Header lines (prompt + section labels) shift the
    /// item-index → line-index relationship, so we track it here.
    fn build_lines(&mut self, width: u16) -> (Vec<Line<'static>>, u16) {
        let theme = crate::theme::current();
        let mut lines: Vec<Line> = Vec::with_capacity(self.items.len() + 4);
        let mut cursor_line: u16 = 0;
        // Prompt — split on '\n' so each prompt line is its own `Line`.
        // Without this, ratatui's wrap reflows the embedded newlines
        // into a single rendered row count that doesn't match what we
        // tracked for the cursor, producing an off-by-N scroll bug
        // (cursor lands one row below the body when scrolling near
        // the bottom).
        let prompt_style = Style::default().fg(theme.text_dim);
        for segment in self.prompt.split('\n') {
            lines.push(Line::from(Span::styled(segment.to_string(), prompt_style)));
        }
        lines.push(Line::raw(""));

        // Section grouping — if a `section_for` exists, walk the
        // items printing the section header before the first item of
        // each group.
        let mut last_section: Option<&'static str> = None;
        for (i, item) in self.items.iter().enumerate() {
            if let Some(sec_fn) = self.section_for.as_ref() {
                let section = sec_fn(item);
                if !section.is_empty() && Some(section) != last_section {
                    if last_section.is_some() {
                        lines.push(Line::raw(""));
                    }
                    // Truncate the same way item rows are — wrap is
                    // off, so an overlong section label would print
                    // off the modal's right edge otherwise.
                    let section_truncated = if section.chars().count() > width as usize {
                        let mut s: String = section.chars().take(width as usize - 1).collect();
                        s.push('…');
                        s
                    } else {
                        section.to_string()
                    };
                    lines.push(Line::from(Span::styled(
                        section_truncated,
                        Style::default().fg(theme.warn).add_modifier(Modifier::BOLD),
                    )));
                    last_section = Some(section);
                }
            }
            let is_cursor = i == self.cursor;
            let selectable = self.is_selectable(i);
            let selected = self.selected.get(i).copied().unwrap_or(false);
            let prefix = match (self.mode, selected, selectable) {
                (Mode::Multi, true, true) => "[x] ",
                (Mode::Multi, false, true) => "[ ] ",
                (Mode::Multi, _, false) => "[·] ",
                (Mode::Single, _, true) => "    ",
                (Mode::Single, _, false) => "    ",
            };
            let cursor_caret = if is_cursor { "▸ " } else { "  " };
            let mut style = if !selectable {
                Style::default().fg(theme.text_dim)
            } else if is_cursor {
                Style::default()
                    .fg(theme.text_strong)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(theme.text_strong)
            };
            if is_cursor {
                style = style.bg(theme.fill);
            }
            let label = (self.label_for)(item);
            let line = format!("{cursor_caret}{prefix}{label}");
            // Truncate to width.
            let truncated = if line.chars().count() > width as usize {
                let mut s: String = line.chars().take(width as usize - 1).collect();
                s.push('…');
                s
            } else {
                line
            };
            if is_cursor {
                cursor_line = lines.len() as u16;
            }
            lines.push(Line::from(Span::styled(truncated, style)));
        }
        // Empty hint
        if self.show_empty_hint {
            lines.push(Line::raw(""));
            lines.push(Line::from(Span::styled(
                "  pick at least one (Space to toggle)",
                Style::default().fg(theme.error),
            )));
        }
        (lines, cursor_line)
    }
}

enum ConfirmResult {
    Stay,
    Cancel,
    Picked(Vec<ChoicePayload>),
}

impl<T: Clone + 'static + Send> Component for Choice<T> {
    fn view(&mut self, frame: &mut Frame, area: Rect) {
        let theme = crate::theme::current();
        let modal_w = 80u16.min(area.width.saturating_sub(4));
        // An empty list has no rows to show, so a full-height modal
        // would render as a large blank rectangle over the panes —
        // the "black screen" from issue #35. Size the box to the
        // prompt instead so the empty state reads as a small framed
        // notice. `+ 5` covers the blank line after the prompt, the
        // two-row body/help gap, and the borders.
        let modal_h = if self.items.is_empty() {
            self.prompt.split('\n').count() as u16 + 5
        } else {
            24
        }
        .min(area.height.saturating_sub(4));
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

        let (lines, cursor_line) = self.build_lines(inner.width);
        // Help footer — an empty list can only be dismissed, so drop
        // the navigate/toggle/confirm hints that don't apply.
        let help_spans = if self.items.is_empty() {
            vec![
                Span::styled("Esc", Style::default().fg(theme.error).bold()),
                Span::raw("/"),
                Span::styled("Enter", Style::default().fg(theme.accent).bold()),
                Span::raw(" close"),
            ]
        } else {
            let mut help_spans = vec![
                Span::styled("↑↓", Style::default().fg(theme.accent).bold()),
                Span::raw(" navigate  "),
            ];
            if self.mode == Mode::Multi {
                help_spans.push(Span::styled(
                    "Space",
                    Style::default().fg(theme.accent).bold(),
                ));
                help_spans.push(Span::raw(" toggle  "));
            }
            help_spans.push(Span::styled(
                "Enter",
                Style::default().fg(theme.success).bold(),
            ));
            help_spans.push(Span::raw(" confirm  "));
            if self.can_refresh {
                help_spans.push(Span::styled("r", Style::default().fg(theme.warn).bold()));
                help_spans.push(Span::raw(" refresh  "));
            }
            if self.can_back {
                help_spans.push(Span::styled(
                    "Backspace",
                    Style::default().fg(theme.warn).bold(),
                ));
                help_spans.push(Span::raw(" back  "));
            }
            help_spans.push(Span::styled("Esc", Style::default().fg(theme.error).bold()));
            help_spans.push(Span::raw(" cancel"));
            help_spans
        };

        // Layout: lines occupy inner.height-2 rows; help at bottom
        let help_area = Rect {
            x: inner.x,
            y: inner.y + inner.height - 1,
            width: inner.width,
            height: 1,
        };
        let body_area = Rect {
            x: inner.x,
            y: inner.y,
            width: inner.width,
            height: inner.height - 2,
        };
        // Adjust the persistent scroll offset so the cursor row stays
        // within `body_area`. Only nudges when the cursor walks past
        // either edge — typing j/k inside the visible window leaves
        // the offset alone, so the list doesn't drift unnecessarily.
        let body_h = body_area.height;
        // Cache for PageUp/PageDown jump size. `body_area` isn't
        // visible in `on()` so we stash it here.
        self.body_height = body_h.max(1);
        if body_h > 0 {
            if cursor_line < self.scroll {
                self.scroll = cursor_line;
            } else if cursor_line >= self.scroll + body_h {
                self.scroll = cursor_line + 1 - body_h;
            }
            // Don't scroll past the last line — keeps blank rows
            // from showing when the list is short.
            let total = lines.len() as u16;
            let max_scroll = total.saturating_sub(body_h);
            if self.scroll > max_scroll {
                self.scroll = max_scroll;
            }
        }
        // No wrap — each Line is already truncated to `inner.width`
        // in `build_lines`, so line index === terminal row. That's
        // load-bearing for the scroll math above.
        frame.render_widget(Paragraph::new(lines).scroll((self.scroll, 0)), body_area);
        frame.render_widget(Paragraph::new(Line::from(help_spans)), help_area);
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

impl<T: Clone + 'static + Send> AppComponent<Msg, UserEvent> for Choice<T> {
    fn on(&mut self, ev: &Event<UserEvent>) -> Option<Msg> {
        let Event::Keyboard(key) = ev else {
            return None;
        };
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        if matches!(key.code, Key::Esc) || (ctrl && matches!(key.code, Key::Char('c'))) {
            return Some(Msg::ModalDismissed);
        }
        // Fire the live-preview callback once after the match if the
        // cursor moved — covers every navigation arm (arrows, page,
        // half-page, Home/End) without repeating the call in each.
        let prev_cursor = self.cursor;
        let result = match key.code {
            Key::Down => {
                self.move_cursor(1);
                self.show_empty_hint = false;
                None
            }
            Key::Up => {
                self.move_cursor(-1);
                self.show_empty_hint = false;
                None
            }
            Key::PageDown => {
                self.move_cursor(self.body_height as isize);
                self.show_empty_hint = false;
                None
            }
            Key::PageUp => {
                self.move_cursor(-(self.body_height as isize));
                self.show_empty_hint = false;
                None
            }
            // Ctrl-d / Ctrl-u — half-page jump, vim-style. Useful
            // when keyboards don't have PageUp/PageDown surfaced.
            Key::Char('d') if ctrl => {
                self.move_cursor((self.body_height / 2).max(1) as isize);
                self.show_empty_hint = false;
                None
            }
            Key::Char('u') if ctrl => {
                self.move_cursor(-((self.body_height / 2).max(1) as isize));
                self.show_empty_hint = false;
                None
            }
            Key::Home | Key::Char('g') => {
                self.cursor_to_first();
                self.show_empty_hint = false;
                None
            }
            Key::End | Key::Char('G') => {
                self.cursor_to_last();
                self.show_empty_hint = false;
                None
            }
            Key::Char(' ') if self.mode == Mode::Multi => {
                if !self.items.is_empty() && self.is_selectable(self.cursor) {
                    self.selected[self.cursor] = !self.selected[self.cursor];
                }
                self.show_empty_hint = false;
                None
            }
            Key::Char('r') if self.can_refresh => Some(Msg::ChoiceRefresh),
            Key::Backspace if self.can_back => Some(Msg::ChoiceBack),
            Key::Enter => match self.confirm_picks() {
                ConfirmResult::Stay => None,
                ConfirmResult::Cancel => Some(Msg::ModalDismissed),
                ConfirmResult::Picked(picks) => Some(Msg::ChoicePicked(picks)),
            },
            _ => None,
        };
        if self.cursor != prev_cursor {
            self.fire_highlight();
        }
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Tiny payload — exercising `Choice` only needs cloneable items
    /// with stable equality for assertions.
    #[derive(Clone, PartialEq, Eq, Debug)]
    struct Item(&'static str);

    fn ten() -> Choice<Item> {
        let items: Vec<Item> = (0..10)
            .map(|i| Item(Box::leak(format!("i{i}").into_boxed_str())))
            .collect();
        Choice::single("pick", items)
    }

    #[test]
    fn on_highlight_fires_at_mount_and_on_move() {
        use std::sync::{Arc, Mutex};
        use tuirealm::event::{Event, Key, KeyEvent};

        let seen: Arc<Mutex<Vec<&'static str>>> = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&seen);
        let items = vec![Item("a"), Item("b"), Item("c")];
        // Start on row 1; the mount preview should fire for "b".
        let mut c = Choice::single("p", items)
            .select_index(1)
            .on_highlight(move |i: &Item| sink.lock().unwrap().push(i.0));
        assert_eq!(
            *seen.lock().unwrap(),
            vec!["b"],
            "mount previews the start row"
        );

        c.on(&Event::Keyboard(KeyEvent::from(Key::Down))); // → c
        c.on(&Event::Keyboard(KeyEvent::from(Key::Up))); // → b
        c.on(&Event::Keyboard(KeyEvent::from(Key::Up))); // → a (top)
        // Already at the top edge — this move is a no-op and must not
        // re-fire the preview.
        c.on(&Event::Keyboard(KeyEvent::from(Key::Up)));
        assert_eq!(
            *seen.lock().unwrap(),
            vec!["b", "c", "b", "a"],
            "preview fires once per actual cursor move, never on a no-op",
        );
    }

    #[test]
    fn move_cursor_clamps_to_range() {
        let mut c = ten();
        c.move_cursor(-5);
        assert_eq!(c.cursor, 0);
        c.move_cursor(100);
        assert_eq!(c.cursor, 9);
    }

    #[test]
    fn move_cursor_skips_non_selectable_forward() {
        let items: Vec<Item> = vec!["a", "b", "c", "d", "e"]
            .into_iter()
            .map(Item)
            .collect();
        // Mark indices 1 and 2 non-selectable.
        let mut c = Choice::single("p", items).selectable(|i: &Item| !matches!(i.0, "b" | "c"));
        c.cursor = 0;
        c.move_cursor(1);
        // Should hop past b/c and land on d (index 3).
        assert_eq!(c.cursor, 3);
    }

    #[test]
    fn move_cursor_skips_non_selectable_backward() {
        let items: Vec<Item> = vec!["a", "b", "c", "d", "e"]
            .into_iter()
            .map(Item)
            .collect();
        let mut c = Choice::single("p", items).selectable(|i: &Item| !matches!(i.0, "b" | "c"));
        c.cursor = 3;
        c.move_cursor(-1);
        // Should hop past c/b and land on a (index 0).
        assert_eq!(c.cursor, 0);
    }

    #[test]
    fn cursor_to_first_and_last_respect_selectability() {
        let items: Vec<Item> = vec!["a", "b", "c", "d"].into_iter().map(Item).collect();
        // First selectable is 'b'; last selectable is 'c'.
        let mut c = Choice::single("p", items).selectable(|i: &Item| matches!(i.0, "b" | "c"));
        c.cursor_to_last();
        assert_eq!(c.cursor, 2);
        c.cursor_to_first();
        assert_eq!(c.cursor, 1);
    }

    #[test]
    fn multi_confirm_requires_a_tick_by_default() {
        let mut c = Choice::multi("p", vec![Item("a"), Item("b")]);
        // Mode::Multi + require_one (default true) → empty picks → Stay.
        match c.confirm_picks() {
            ConfirmResult::Stay => {}
            other => panic!("expected Stay, got {:?}", core::mem::discriminant(&other)),
        }
        assert!(c.show_empty_hint);
    }

    #[test]
    fn multi_with_allow_empty_returns_picked_on_empty() {
        let mut c = Choice::multi("p", vec![Item("a"), Item("b")]).allow_empty(true);
        match c.confirm_picks() {
            ConfirmResult::Picked(v) if v.is_empty() => {}
            other => panic!(
                "expected empty Picked, got {:?}",
                core::mem::discriminant(&other)
            ),
        }
    }

    #[test]
    fn with_selected_by_pre_ticks() {
        let items = vec![Item("a"), Item("b"), Item("c")];
        let c = Choice::multi("p", items).with_selected_by(|i: &Item| i.0 == "b");
        assert_eq!(c.selected, vec![false, true, false]);
    }

    #[test]
    fn empty_multi_confirm_cancels_instead_of_latching_hint() {
        // Regression for #35: a `require_one` multi with no items must
        // dismiss on Enter, not trap the user on the "pick one" hint.
        let mut c: Choice<Item> = Choice::multi("nothing here", vec![]);
        match c.confirm_picks() {
            ConfirmResult::Cancel => {}
            other => panic!("expected Cancel, got {:?}", core::mem::discriminant(&other)),
        }
        assert!(!c.show_empty_hint);
    }

    #[test]
    fn empty_choice_renders_compact_framed_box_not_full_height() {
        // Regression for #35: an empty picker must size to its prompt
        // so it reads as a small framed notice, not a near-full-height
        // blank rectangle (the reported "black screen").
        use tuirealm::ratatui::Terminal;
        use tuirealm::ratatui::backend::TestBackend;

        let prompt = "line one\nline two";
        let mut c: Choice<Item> = Choice::multi(prompt, vec![]).title("Empty");
        let mut term = Terminal::new(TestBackend::new(120, 40)).unwrap();
        term.draw(|f| c.view(f, f.area())).unwrap();
        let buf = term.backend().buffer().clone();

        // Count rows touched by the modal border (the rounded box).
        let bordered_rows = (0..buf.area.height)
            .filter(|&y| {
                (0..buf.area.width).any(|x| matches!(buf[(x, y)].symbol(), "│" | "╭" | "╰"))
            })
            .count();
        // Prompt is 2 lines → a handful of rows, nowhere near the
        // full-list height of 24 the old code always used.
        assert!(
            bordered_rows <= 9,
            "empty box should be compact, spanned {bordered_rows} rows",
        );
        assert!(bordered_rows >= 4, "box must still be a visible frame");
    }
}
