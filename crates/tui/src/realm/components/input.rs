//! `Input` — single-line text input. tuirealm port of
//! `tui_kit::widgets::InputModal`.
//!
//! Returns `Msg::InputSubmitted(text)` on Enter (when valid),
//! `Msg::ModalDismissed` on Esc.

use crate::realm::Msg;
use crate::realm::UserEvent;
use tuirealm::command::{Cmd, CmdResult};
use tuirealm::component::{AppComponent, Component};
use tuirealm::event::{Event, Key, KeyEvent, KeyModifiers};
use tuirealm::props::{AttrValue, Attribute, QueryResult};
use tuirealm::ratatui::Frame;
use tuirealm::ratatui::layout::Rect;
use tuirealm::ratatui::prelude::*;
use tuirealm::ratatui::widgets::{Block, BorderType, Borders, Clear, Paragraph, Wrap};
use tuirealm::state::State;

type Validator = Box<dyn Fn(&str) -> bool + Send + Sync>;

/// Single-line text input.
pub struct Input {
    presentation: crate::realm::presentation::Presentation,
    title: String,
    prompt: String,
    placeholder: String,
    input: String,
    validator: Option<Validator>,
    /// Asterisk-mask the typed characters. For tokens / passwords.
    secret: bool,
}

impl Input {
    fn view_mobile(&self, frame: &mut Frame, area: Rect) {
        let theme = crate::theme::current();
        let modal = self.presentation.modal(area, 60, 9);
        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .title(format!(" {} ", self.title))
            .border_style(theme.modal_border());
        let inner = block.inner(modal);
        frame.render_widget(Clear, modal);
        frame.render_widget(block, modal);
        if inner.height < 3 {
            return;
        }
        let prompt = Rect::new(
            inner.x,
            inner.y,
            inner.width,
            inner.height.saturating_sub(3),
        );
        frame.render_widget(
            Paragraph::new(self.prompt.as_str())
                .wrap(Wrap { trim: false })
                .style(Style::default().fg(theme.text_dim)),
            prompt,
        );
        let display = self.display_string();
        let mut start = 0;
        while crate::util::visual_width(&display[start..]) > usize::from(inner.width) {
            let Some(ch) = display[start..].chars().next() else {
                break;
            };
            start += ch.len_utf8();
        }
        frame.render_widget(
            Paragraph::new(&display[start..]).style(Style::default().fg(theme.text_strong)),
            Rect::new(inner.x, inner.bottom() - 3, inner.width, 1),
        );
        frame.render_widget(
            Paragraph::new("Ctrl-X clear").style(Style::default().fg(theme.text_dim)),
            Rect::new(inner.x, inner.bottom() - 2, inner.width, 1),
        );
        frame.render_widget(
            Paragraph::new("Enter save  Esc cancel").style(Style::default().fg(theme.accent)),
            Rect::new(inner.x, inner.bottom() - 1, inner.width, 1),
        );
    }

    /// Build a prompt asking for `prompt`.
    pub fn new(prompt: impl Into<String>) -> Self {
        Self {
            presentation: crate::realm::presentation::Presentation::Desktop,
            title: "Input".to_string(),
            prompt: prompt.into(),
            placeholder: String::new(),
            input: String::new(),
            validator: None,
            secret: false,
        }
    }

    /// Override the modal title.
    pub fn title(mut self, title: impl Into<String>) -> Self {
        self.title = title.into();
        self
    }

    /// Placeholder shown dim when input is empty.
    pub fn placeholder(mut self, ph: impl Into<String>) -> Self {
        self.placeholder = ph.into();
        self
    }

    /// Pre-fill the input.
    pub fn with_input(mut self, text: impl Into<String>) -> Self {
        self.input = text.into();
        self
    }

    /// Hide typed chars behind asterisks.
    pub fn secret(mut self) -> Self {
        self.secret = true;
        self
    }

    /// Gate Enter on a validator. Empty input is always invalid.
    pub fn with_validator<F>(mut self, f: F) -> Self
    where
        F: Fn(&str) -> bool + Send + Sync + 'static,
    {
        self.validator = Some(Box::new(f));
        self
    }

    fn is_valid(&self) -> bool {
        if self.input.is_empty() {
            return false;
        }
        self.validator
            .as_ref()
            .map(|v| v(&self.input))
            .unwrap_or(true)
    }

    fn display_string(&self) -> String {
        if self.input.is_empty() {
            return if self.placeholder.is_empty() {
                "▌".to_string()
            } else {
                format!("▌ {}", self.placeholder)
            };
        }
        let body = if self.secret {
            "•".repeat(self.input.chars().count())
        } else {
            self.input.clone()
        };
        format!("{body}▌")
    }
}

impl Component for Input {
    fn view(&mut self, frame: &mut Frame, area: Rect) {
        if self.presentation == crate::realm::presentation::Presentation::Mobile {
            self.view_mobile(frame, area);
            return;
        }
        let theme = crate::theme::current();
        let modal_w = 60u16.min(area.width.saturating_sub(4));
        let modal_h = 7u16;
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

        let valid = self.is_valid();
        let prompt_line = Line::from(Span::styled(
            self.prompt.clone(),
            Style::default().fg(theme.text_dim),
        ));
        let display = self.display_string();
        let input_style = if self.input.is_empty() {
            theme.hint()
        } else if valid {
            Style::default().fg(theme.text_strong)
        } else {
            Style::default().fg(theme.error)
        };
        let input_line = Line::from(Span::styled(display, input_style));
        let help = Line::from(vec![
            Span::styled("Enter", Style::default().fg(theme.success).bold()),
            Span::raw(" submit  "),
            Span::styled("Esc", Style::default().fg(theme.error).bold()),
            Span::raw(" cancel"),
        ]);
        frame.render_widget(
            Paragraph::new(vec![
                prompt_line,
                Line::raw(""),
                input_line,
                Line::raw(""),
                help,
            ])
            .wrap(Wrap { trim: false }),
            inner,
        );
    }

    fn query(&self, _: Attribute) -> Option<QueryResult<'_>> {
        None
    }
    fn attr(&mut self, attr: Attribute, value: AttrValue) {
        self.presentation.apply_attribute(attr, value);
    }
    fn state(&self) -> State {
        State::None
    }
    fn perform(&mut self, _: Cmd) -> CmdResult {
        CmdResult::NoChange
    }
}

impl AppComponent<Msg, UserEvent> for Input {
    fn on(&mut self, ev: &Event<UserEvent>) -> Option<Msg> {
        match ev {
            Event::Keyboard(KeyEvent { code: Key::Esc, .. }) => Some(Msg::ModalDismissed),
            Event::Keyboard(KeyEvent {
                code: Key::Char('c'),
                modifiers,
                ..
            }) if modifiers.contains(KeyModifiers::CONTROL) => Some(Msg::ModalDismissed),
            Event::Keyboard(KeyEvent {
                code: Key::Char('x'),
                modifiers,
                ..
            }) if self.presentation == crate::realm::presentation::Presentation::Mobile
                && *modifiers == KeyModifiers::CONTROL =>
            {
                self.input.clear();
                None
            }
            Event::Keyboard(KeyEvent {
                code: Key::Enter, ..
            }) => {
                if self.is_valid() {
                    Some(Msg::InputSubmitted(self.input.clone()))
                } else {
                    None
                }
            }
            Event::Keyboard(KeyEvent {
                code: Key::Backspace,
                ..
            }) => {
                self.input.pop();
                None
            }
            Event::Keyboard(KeyEvent {
                code: Key::Char(c),
                modifiers,
                ..
            }) if !modifiers.contains(KeyModifiers::CONTROL) => {
                self.input.push(*c);
                None
            }
            _ => None,
        }
    }
}
