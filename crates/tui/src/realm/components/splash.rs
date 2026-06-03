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
        // commands" block (issue #25) lists the always-available
        // global shortcuts here so the per-view footer doesn't need
        // to repeat them on every screen. The list is built from the
        // catalog (`ActionDef::all()`) so a rename/rebind here flows
        // through automatically — same single source of truth that
        // drives the footer + `?` help modal.
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
                "    Tour of commands — always available:",
                Style::default()
                    .fg(theme.text_strong)
                    .add_modifier(Modifier::BOLD),
            )),
        ];
        for (keys, label) in universal_shortcuts() {
            lines.push(Line::from(vec![
                Span::styled("      ", Style::default()),
                Span::styled(
                    format!("{keys:<14}"),
                    Style::default()
                        .fg(theme.accent)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled("  ", Style::default()),
                Span::styled(label, Style::default().fg(theme.text_dim)),
            ]));
        }
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

/// The "always available" shortcuts surfaced in the onboarding tour
/// + `?` help modal. Reads from the catalog so footer / help / tour
/// stay in lockstep — the single source of truth is `ActionDef`.
///
/// Returns `(default_keys, label)` pairs. We pick the six globals
/// that matter for first-run discoverability: help, quit, settings,
/// refresh, cycle pane, detach. The Resize splitter binding is
/// available but not listed in the tour to keep the card scannable;
/// the `?` help modal lists every global.
pub fn universal_shortcuts() -> Vec<(&'static str, &'static str)> {
    use lazybox_tui_core::action::{ActionDef, ActionKind};
    [
        ActionKind::OpenHelp,
        ActionKind::Quit,
        ActionKind::OpenSettings,
        ActionKind::Refresh,
        ActionKind::CyclePane,
        ActionKind::DetachPane,
    ]
    .into_iter()
    .map(|k| {
        let def = ActionDef::for_kind(k);
        (def.default_keys, def.label)
    })
    .collect()
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
