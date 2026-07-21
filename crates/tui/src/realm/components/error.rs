//! `ErrorModal` — diagnostic with a colored severity pill. tuirealm
//! port of `tui_kit::widgets::ErrorModal`.
//!
//! Diagnostics dismiss on any key by default. Callers that require an explicit
//! acknowledgement can restrict dismissal to Esc or Enter.

use crate::realm::Msg;
use crate::realm::UserEvent;
use ratatui::style::Color;
use tuirealm::command::{Cmd, CmdResult};
use tuirealm::component::{AppComponent, Component};
use tuirealm::event::{Event, Key, KeyEvent};
use tuirealm::props::{AttrValue, Attribute, QueryResult};
use tuirealm::ratatui::Frame;
use tuirealm::ratatui::layout::Rect;
use tuirealm::ratatui::prelude::*;
use tuirealm::ratatui::widgets::{Block, BorderType, Borders, Clear, Paragraph, Wrap};
use tuirealm::state::State;

/// Severity tag for the modal — short label + color.
#[derive(Debug, Clone)]
pub struct Accent {
    /// Short word shown inside the pill.
    pub label: String,
    /// Pill background + border tint.
    pub color: Color,
}

impl Accent {
    /// Construct any accent.
    pub fn new(label: impl Into<String>, color: Color) -> Self {
        Self {
            label: label.into(),
            color,
        }
    }

    /// Transient hiccup — uses `theme.warn`.
    pub fn warn(label: impl Into<String>) -> Self {
        Self::new(label, crate::theme::current().warn)
    }

    /// Hard failure — uses `theme.error`.
    pub fn error(label: impl Into<String>) -> Self {
        Self::new(label, crate::theme::current().error)
    }

    /// Heads-up but not blocking — uses `theme.accent`.
    pub fn info(label: impl Into<String>) -> Self {
        Self::new(label, crate::theme::current().accent)
    }
}

/// Diagnostic modal.
pub struct ErrorModal {
    title: String,
    source: String,
    accent: Accent,
    detail: String,
    dismiss_on_confirm: bool,
}

impl ErrorModal {
    /// Build a modal showing `detail` from `source` with severity
    /// `accent`. Title defaults to "Error".
    pub fn new(source: impl Into<String>, accent: Accent, detail: impl Into<String>) -> Self {
        Self {
            title: "Error".to_string(),
            source: source.into(),
            accent,
            detail: detail.into(),
            dismiss_on_confirm: false,
        }
    }

    /// Override the title.
    pub fn title(mut self, title: impl Into<String>) -> Self {
        self.title = title.into();
        self
    }

    /// Require Esc or Enter instead of dismissing on any keyboard input.
    pub fn dismiss_on_confirm(mut self) -> Self {
        self.dismiss_on_confirm = true;
        self
    }
}

impl Component for ErrorModal {
    fn view(&mut self, frame: &mut Frame, area: Rect) {
        let theme = crate::theme::current();
        let modal_w = 90u16.min(area.width.saturating_sub(4));
        let modal_h = 22u16.min(area.height.saturating_sub(2));
        let x = area.x + area.width.saturating_sub(modal_w) / 2;
        let y = area.y + area.height.saturating_sub(modal_h) / 2;
        let modal = Rect::new(x, y, modal_w, modal_h);

        frame.render_widget(Clear, modal);
        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(self.accent.color));
        let inner = block.inner(modal);
        frame.render_widget(block, modal);

        const POWERLINE_RIGHT: &str = "\u{e0b0}";
        let header = Line::from(vec![
            Span::styled(
                format!(" {} ", self.accent.label),
                Style::default()
                    .bg(self.accent.color)
                    .fg(Color::Black)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(POWERLINE_RIGHT, Style::default().fg(self.accent.color)),
            Span::raw(" "),
            Span::styled(
                self.source.clone(),
                Style::default()
                    .fg(theme.text_strong)
                    .add_modifier(Modifier::BOLD),
            ),
        ]);

        let mut lines = vec![header, Line::raw("")];
        for raw in self.detail.lines() {
            lines.push(Line::from(Span::styled(
                raw.to_string(),
                Style::default().fg(theme.text_dim),
            )));
        }
        lines.push(Line::raw(""));
        lines.push(Line::from(vec![Span::styled(
            if self.dismiss_on_confirm {
                "Press Esc or Enter to dismiss"
            } else {
                "Press any key to dismiss"
            },
            theme.hint(),
        )]));
        frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), inner);
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

impl AppComponent<Msg, UserEvent> for ErrorModal {
    fn on(&mut self, ev: &Event<UserEvent>) -> Option<Msg> {
        match ev {
            Event::Keyboard(KeyEvent {
                code: Key::Esc | Key::Enter,
                ..
            }) => Some(Msg::ModalDismissed),
            Event::Keyboard(_) if !self.dismiss_on_confirm => Some(Msg::ModalDismissed),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tuirealm::event::KeyModifiers;

    fn key(code: Key) -> Event<UserEvent> {
        Event::Keyboard(KeyEvent::new(code, KeyModifiers::NONE))
    }

    #[test]
    fn explicit_dismissal_ignores_unrelated_keys() {
        let mut modal =
            ErrorModal::new("Update", Accent::info("UPDATE"), "detail").dismiss_on_confirm();

        assert_eq!(modal.on(&key(Key::Char('j'))), None);
        assert_eq!(modal.on(&key(Key::Esc)), Some(Msg::ModalDismissed));
        assert_eq!(modal.on(&key(Key::Enter)), Some(Msg::ModalDismissed));
    }

    #[test]
    fn diagnostic_default_still_dismisses_on_any_key() {
        let mut modal = ErrorModal::new("Error", Accent::error("ERROR"), "detail");
        assert_eq!(modal.on(&key(Key::Char('j'))), Some(Msg::ModalDismissed));
    }
}
