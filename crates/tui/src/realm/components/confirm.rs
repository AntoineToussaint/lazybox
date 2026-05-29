//! `Confirm` — yes/no prompt. tuirealm port of
//! `tui_kit::widgets::ConfirmModal`.
//!
//! Returns `Msg::Confirmed(true)` on Y/Enter, `Msg::Confirmed(false)`
//! on N. Esc maps to `Msg::ModalDismissed`. Unlike the tui-kit
//! version, the boolean lives inside `Msg` rather than being passed
//! via a generic `Done(Box<Any>)` payload — that's the whole point of
//! tuirealm's typed Msg approach.

use crate::realm::DOUBLE_CLICK_WINDOW;
use crate::realm::Msg;
use crate::realm::UserEvent;
use std::time::Instant;
use tuirealm::command::{Cmd, CmdResult};
use tuirealm::component::{AppComponent, Component};
use tuirealm::event::{
    Event, Key, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use tuirealm::props::{AttrValue, Attribute, QueryResult};
use tuirealm::ratatui::Frame;
use tuirealm::ratatui::layout::{Position, Rect};
use tuirealm::ratatui::prelude::*;
use tuirealm::ratatui::widgets::{Block, BorderType, Borders, Clear, Paragraph, Wrap};
use tuirealm::state::State;

/// Y/N confirmation prompt.
pub struct Confirm {
    question: String,
    /// Currently-highlighted option. True = Yes, false = No. Initialized
    /// from `default_yes` (or `default_no`); ←/→ arrows flip it; Enter
    /// fires the highlighted side.
    selected_yes: bool,
    /// Screen rect of the `[Y]es` / `[N]o` buttons, recorded on each
    /// `view()` so mouse clicks can be hit-tested against them.
    yes_rect: Option<Rect>,
    no_rect: Option<Rect>,
    /// Last button clicked and when, for double-click detection: which
    /// side (true = Yes) plus the click instant.
    last_click: Option<(bool, Instant)>,
}

impl Confirm {
    /// Build a prompt asking `question`. Defaults `Enter` to yes.
    pub fn new(question: impl Into<String>) -> Self {
        Self {
            question: question.into(),
            selected_yes: true,
            yes_rect: None,
            no_rect: None,
            last_click: None,
        }
    }

    /// Make `Enter` default to "no". Use for destructive prompts where
    /// the safer option is to back out.
    pub fn default_no(mut self) -> Self {
        self.selected_yes = false;
        self
    }
}

impl Component for Confirm {
    fn view(&mut self, frame: &mut Frame, area: Rect) {
        let theme = crate::theme::current();
        // Width: prefer wide so most questions fit on one line, but
        // clamp to the available area.
        let modal_w = 80u16.min(area.width.saturating_sub(4)).max(20);
        let inner_w = modal_w.saturating_sub(2).max(1) as usize;

        // Height: 1 empty + N question lines + 1 empty + 1 buttons +
        // 2 borders. Estimate N by character-count over inner_w —
        // ratatui's `Wrap { trim: false }` is word-wrap with
        // character-break fallback, so dividing total chars by width
        // is a safe upper bound. Without this dynamic sizing, the
        // hardcoded 6-row modal hid the Y/N buttons whenever the
        // question wrapped past one line, leaving the user stuck.
        let q_chars = self.question.chars().count();
        let q_lines = q_chars.div_ceil(inner_w).max(1) as u16;
        let modal_h = (5 + q_lines).min(area.height.saturating_sub(2)).max(6);
        let x = area.x + area.width.saturating_sub(modal_w) / 2;
        let y = area.y + area.height.saturating_sub(modal_h) / 2;
        let modal = Rect::new(x, y, modal_w, modal_h);

        frame.render_widget(Clear, modal);
        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .title(Span::styled(" Confirm ", theme.modal_title()))
            .border_style(theme.modal_border());
        let inner = block.inner(modal);
        frame.render_widget(block, modal);

        let yes_style = if self.selected_yes {
            Style::default()
                .fg(theme.success)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(theme.text_dim)
        };
        let no_style = if self.selected_yes {
            Style::default().fg(theme.text_dim)
        } else {
            Style::default()
                .fg(theme.error)
                .add_modifier(Modifier::BOLD)
        };

        // Pin the buttons to the last inner row and let the question
        // fill everything above it (after a one-row top pad). The
        // question's wrapped height is only an upper-bound estimate, so
        // anchoring the buttons to the bottom keeps their click targets
        // at a fixed, hit-testable position no matter how it wraps.
        let buttons_row = inner.y + inner.height.saturating_sub(1);
        let question_area = Rect::new(
            inner.x,
            inner.y + 1,
            inner.width,
            inner.height.saturating_sub(2),
        );
        frame.render_widget(
            Paragraph::new(self.question.as_str()).wrap(Wrap { trim: false }),
            question_area,
        );

        let buttons = Line::from(vec![
            Span::styled("[Y]es", yes_style),
            Span::raw("    "),
            Span::styled("[N]o", no_style),
            Span::raw("    "),
            Span::styled("← / →", theme.hint()),
            Span::raw("  "),
            Span::styled("Esc cancel", theme.hint()),
        ]);
        frame.render_widget(
            Paragraph::new(buttons),
            Rect::new(inner.x, buttons_row, inner.width, 1),
        );

        // `[Y]es` is 5 cells wide at the left edge; `[N]o` (4 cells)
        // follows after a 4-cell gap.
        self.yes_rect = Some(Rect::new(inner.x, buttons_row, 5, 1));
        self.no_rect = Some(Rect::new(inner.x + 9, buttons_row, 4, 1));
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

impl AppComponent<Msg, UserEvent> for Confirm {
    fn on(&mut self, ev: &Event<UserEvent>) -> Option<Msg> {
        match ev {
            Event::Keyboard(KeyEvent { code: Key::Esc, .. }) => Some(Msg::ModalDismissed),
            Event::Keyboard(KeyEvent {
                code: Key::Char('c'),
                modifiers,
                ..
            }) if modifiers.contains(KeyModifiers::CONTROL) => Some(Msg::ModalDismissed),
            Event::Keyboard(KeyEvent {
                code: Key::Char('y') | Key::Char('Y'),
                ..
            }) => Some(Msg::Confirmed(true)),
            Event::Keyboard(KeyEvent {
                code: Key::Char('n') | Key::Char('N'),
                ..
            }) => Some(Msg::Confirmed(false)),
            // ← highlights Yes (left side), → highlights No (right
            // side). Also accept Vim-style h/l for keyboard-first
            // users. Principle of least surprise: in any UI that
            // shows two options side-by-side, arrows should toggle.
            Event::Keyboard(KeyEvent {
                code: Key::Left | Key::Char('h'),
                ..
            }) => {
                self.selected_yes = true;
                None
            }
            Event::Keyboard(KeyEvent {
                code: Key::Right | Key::Char('l'),
                ..
            }) => {
                self.selected_yes = false;
                None
            }
            // Tab also toggles — pairs well with single-handed use.
            Event::Keyboard(KeyEvent { code: Key::Tab, .. }) => {
                self.selected_yes = !self.selected_yes;
                None
            }
            Event::Keyboard(KeyEvent {
                code: Key::Enter, ..
            }) => Some(Msg::Confirmed(self.selected_yes)),
            // Left-click on a button selects it; a second click on the
            // same button within the double-click window fires it —
            // the mouse equivalent of highlighting then pressing Enter.
            Event::Mouse(MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column,
                row,
                ..
            }) => {
                let at = Position::new(*column, *row);
                let hits = |r: Option<Rect>| r.is_some_and(|r| r.contains(at));
                let side = if hits(self.yes_rect) {
                    true
                } else if hits(self.no_rect) {
                    false
                } else {
                    return None;
                };
                self.selected_yes = side;
                let is_double = self
                    .last_click
                    .is_some_and(|(prev, t)| prev == side && t.elapsed() <= DOUBLE_CLICK_WINDOW);
                if is_double {
                    self.last_click = None;
                    Some(Msg::Confirmed(side))
                } else {
                    self.last_click = Some((side, Instant::now()));
                    None
                }
            }
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tuirealm::ratatui::Terminal;
    use tuirealm::ratatui::backend::TestBackend;

    /// Render once into a throwaway backend so `view()` records the
    /// button hit-boxes that the mouse handler reads.
    fn render(confirm: &mut Confirm) {
        let mut terminal = Terminal::new(TestBackend::new(100, 30)).unwrap();
        terminal.draw(|f| confirm.view(f, f.area())).unwrap();
    }

    fn left_click(col: u16, row: u16) -> Event<UserEvent> {
        Event::Mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            modifiers: KeyModifiers::empty(),
            column: col,
            row,
        })
    }

    #[test]
    fn single_click_selects_button_without_confirming() {
        let mut c = Confirm::new("Proceed?");
        render(&mut c);
        assert!(c.selected_yes, "defaults to Yes");
        let no = c.no_rect.expect("No rect recorded on view");
        assert_eq!(c.on(&left_click(no.x, no.y)), None);
        assert!(!c.selected_yes, "single click moves the highlight to No");
    }

    #[test]
    fn double_click_confirms_the_clicked_button() {
        let mut c = Confirm::new("Proceed?");
        render(&mut c);
        let no = c.no_rect.expect("No rect recorded on view");
        assert_eq!(c.on(&left_click(no.x, no.y)), None);
        assert_eq!(
            c.on(&left_click(no.x, no.y)),
            Some(Msg::Confirmed(false)),
            "second click on No fires it"
        );

        let mut c = Confirm::new("Proceed?");
        render(&mut c);
        let yes = c.yes_rect.expect("Yes rect recorded on view");
        assert_eq!(c.on(&left_click(yes.x, yes.y)), None);
        assert_eq!(
            c.on(&left_click(yes.x, yes.y)),
            Some(Msg::Confirmed(true)),
            "second click on Yes fires it"
        );
    }

    #[test]
    fn double_click_on_different_buttons_does_not_confirm() {
        let mut c = Confirm::new("Proceed?");
        render(&mut c);
        let yes = c.yes_rect.expect("Yes rect");
        let no = c.no_rect.expect("No rect");
        assert_eq!(c.on(&left_click(yes.x, yes.y)), None);
        // Switching sides resets the pair — no accidental confirm.
        assert_eq!(c.on(&left_click(no.x, no.y)), None);
        assert!(!c.selected_yes);
    }

    #[test]
    fn click_outside_buttons_is_ignored() {
        let mut c = Confirm::new("Proceed?");
        render(&mut c);
        assert_eq!(c.on(&left_click(0, 0)), None);
        assert!(c.last_click.is_none());
    }
}
