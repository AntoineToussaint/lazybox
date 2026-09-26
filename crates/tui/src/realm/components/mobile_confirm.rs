//! Compact explicit-answer confirmation; typing Enter cannot delete a session.
use crate::realm::{Msg, UserEvent, presentation::render_reader};
use tuirealm::{
    command::{Cmd, CmdResult},
    component::{AppComponent, Component},
    event::{Event, Key, KeyEvent, KeyModifiers},
    props::{AttrValue, Attribute, QueryResult},
    ratatui::{Frame, layout::Rect},
    state::State,
};

pub(crate) struct MobileConfirm {
    question: String,
    scroll: u16,
}
impl MobileConfirm {
    pub(crate) fn new(question: String) -> Self {
        Self {
            question,
            scroll: 0,
        }
    }
}
impl Component for MobileConfirm {
    fn view(&mut self, frame: &mut Frame, area: Rect) {
        render_reader(
            frame,
            area,
            "Delete session?",
            &self.question,
            &mut self.scroll,
            "y delete  n/Enter/Esc cancel",
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
impl AppComponent<Msg, UserEvent> for MobileConfirm {
    fn on(&mut self, ev: &Event<UserEvent>) -> Option<Msg> {
        let Event::Keyboard(KeyEvent { code, modifiers }) = ev else {
            return None;
        };
        if *code == Key::Esc
            || (*code == Key::Char('c') && modifiers.contains(KeyModifiers::CONTROL))
        {
            return Some(Msg::ModalDismissed);
        }
        if modifiers.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) {
            return None;
        }
        match code {
            Key::Char('y' | 'Y') => Some(Msg::Confirmed(true)),
            Key::Char('n' | 'N') | Key::Enter => Some(Msg::Confirmed(false)),
            Key::Char('j') | Key::Down => {
                self.scroll = self.scroll.saturating_add(1);
                None
            }
            Key::Char('k') | Key::Up => {
                self.scroll = self.scroll.saturating_sub(1);
                None
            }
            _ => None,
        }
    }
}
