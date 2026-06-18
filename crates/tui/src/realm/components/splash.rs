//! `Splash` — the welcome card. tuirealm port of
//! `crate::components::splash::SplashModal`.
//!
//! Render body is copied verbatim (it was already plain ratatui).
//! Only the trait surface changed: `Modal::handle_key` returning
//! `ModalOutcome` becomes `AppComponent::on` returning `Option<Msg>`.

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

/// Welcome card shown on first run. Press Enter to advance, Esc to
/// quit.
pub struct Splash {
    _private: (),
}

impl Splash {
    /// Construct a fresh splash.
    pub fn new() -> Self {
        Self { _private: () }
    }
}

impl Default for Splash {
    fn default() -> Self {
        Self::new()
    }
}

impl Component for Splash {
    fn view(&mut self, frame: &mut Frame, area: Rect) {
        // The render body — copied from the original SplashModal so
        // lazybox's brand mark + bullets stay identical. The "Tour of
        // commands" block (issue #25) lists the global shortcuts here
        // so the per-view footer doesn't need to repeat them on every
        // screen. The list comes from `action::universal_shortcuts()`
        // so a rename/rebind flows through automatically — the same
        // single source of truth that drives the footer + `?` help
        // modal — and the trailing `]]` note keeps the card honest
        // about the one focus where those globals don't fire (#114).
        let theme = crate::theme::current();
        let modal_w = 64u16.min(area.width.saturating_sub(4));
        let modal_h = 28u16.min(area.height.saturating_sub(2));
        let x = area.x + area.width.saturating_sub(modal_w) / 2;
        let y = area.y + area.height.saturating_sub(modal_h) / 2;
        let modal = Rect::new(x, y, modal_w, modal_h);

        frame.render_widget(Clear, modal);
        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(theme.accent));
        let inner = block.inner(modal);
        frame.render_widget(block, modal);

        let mut lines = vec![
            Line::raw(""),
            Line::from(Span::styled(
                "  lazybox  ",
                Style::default()
                    .fg(theme.accent)
                    .add_modifier(Modifier::BOLD),
            )),
            Line::raw(""),
            Line::from(Span::styled(
                "    A reactive PR inbox in your terminal.",
                Style::default().fg(theme.text_dim),
            )),
            Line::raw(""),
            Line::from(Span::styled(
                "    \u{25c6} Events, not polling: comments, CI, reviews push to you.",
                Style::default().fg(theme.warn),
            )),
            Line::from(Span::styled(
                "    \u{25c6} One session per task, with worktree + agent attached.",
                Style::default().fg(theme.warn),
            )),
            Line::from(Span::styled(
                "    \u{25c6} Source-agnostic: GitHub today, Linear tomorrow.",
                Style::default().fg(theme.warn),
            )),
            Line::raw(""),
            Line::from(Span::styled(
                "    Tour of commands:",
                Style::default()
                    .fg(theme.text_strong)
                    .add_modifier(Modifier::BOLD),
            )),
        ];
        for def in lazybox_tui_core::action::universal_shortcuts() {
            lines.push(Line::from(vec![
                Span::styled("      ", Style::default()),
                Span::styled(
                    format!("{:<14}", def.default_keys),
                    Style::default()
                        .fg(theme.accent)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled("  ", Style::default()),
                Span::styled(def.label, Style::default().fg(theme.text_dim)),
            ]));
        }
        lines.push(Line::raw(""));
        // Honesty about the one focus where the list above stops being
        // "always available": inside an agent/shell terminal every key
        // goes to the PTY, so the globals need the `]]` escape first
        // (issue #114).
        lines.push(Line::from(Span::styled(
            "    In a terminal, press ]] to return here first.",
            Style::default().fg(theme.text_dim),
        )));
        lines.push(Line::raw(""));
        lines.push(Line::from(Span::styled(
            "    Press Enter to begin · Esc to cancel",
            Style::default()
                .fg(theme.success)
                .add_modifier(Modifier::BOLD),
        )));
        frame.render_widget(Paragraph::new(lines), inner);
    }

    fn query(&self, _attr: Attribute) -> Option<QueryResult<'_>> {
        None
    }

    fn attr(&mut self, _attr: Attribute, _value: AttrValue) {}

    fn state(&self) -> State {
        State::None
    }

    fn perform(&mut self, _cmd: Cmd) -> CmdResult {
        CmdResult::NoChange
    }
}

impl AppComponent<Msg, UserEvent> for Splash {
    fn on(&mut self, ev: &Event<UserEvent>) -> Option<Msg> {
        match ev {
            Event::Keyboard(KeyEvent {
                code: Key::Enter, ..
            }) => Some(Msg::SplashConfirmed),
            Event::Keyboard(KeyEvent { code: Key::Esc, .. }) => Some(Msg::AppClose),
            Event::Keyboard(KeyEvent {
                code: Key::Char('c'),
                modifiers,
                ..
            }) if modifiers.contains(KeyModifiers::CONTROL) => Some(Msg::AppClose),
            _ => None,
        }
    }
}
