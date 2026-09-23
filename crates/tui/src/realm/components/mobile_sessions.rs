//! Flat, client-local projection of actual terminals for the minimal phone UI.
use lazybox_core::SessionKey;
use lazybox_ipc::{AgentState, TerminalId};

#[derive(Clone, Debug)]
pub(crate) struct SessionRow {
    pub terminal_id: TerminalId,
    pub session_key: SessionKey,
    pub title: String,
    pub detail: String,
    pub attention: bool,
    pub runner: String,
    pub state: Option<AgentState>,
    pub exited: bool,
}

impl SessionRow {
    pub(crate) fn indicator(&self) -> (&'static str, tuirealm::ratatui::style::Color) {
        let theme = crate::theme::current();
        if self.exited {
            return ("×", theme.text_dim);
        }
        let Some(state) = &self.state else {
            return ("·", theme.accent);
        };
        let glyph = match state {
            AgentState::Working => "●",
            AgentState::Idle => "○",
            AgentState::Done => "✓",
            AgentState::AwaitingReset => "☾",
            AgentState::Exited { .. } => "×",
            _ => "!",
        };
        (
            glyph,
            crate::components::sidebar::agent_state_tone(theme, state),
        )
    }
}

#[derive(Default)]
pub(crate) struct MobileSessions {
    rows: Vec<SessionRow>,
    selected: Option<TerminalId>,
}

impl MobileSessions {
    pub(crate) fn update(&mut self, rows: Vec<SessionRow>) {
        let old_position = self.position();
        self.rows = rows;
        if !self
            .rows
            .iter()
            .any(|r| Some(r.terminal_id) == self.selected)
        {
            self.selected = self
                .rows
                .get(old_position.min(self.rows.len().saturating_sub(1)))
                .map(|r| r.terminal_id);
        }
    }

    pub(crate) fn rows(&self) -> &[SessionRow] {
        &self.rows
    }

    pub(crate) fn len(&self) -> usize {
        self.rows.len()
    }

    fn position(&self) -> usize {
        self.rows
            .iter()
            .position(|r| Some(r.terminal_id) == self.selected)
            .unwrap_or(0)
    }

    pub(crate) fn selected(&self) -> Option<&SessionRow> {
        self.rows.get(self.position())
    }

    pub(crate) fn select(&mut self, id: TerminalId) {
        if self.rows.iter().any(|r| r.terminal_id == id) {
            self.selected = Some(id);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn row(id: u64) -> SessionRow {
        SessionRow {
            terminal_id: TerminalId(id),
            session_key: SessionKey::new(format!("session-{id}")),
            title: format!("Session {id}"),
            detail: "running · shell".into(),
            attention: false,
            runner: "shell".into(),
            state: None,
            exited: false,
        }
    }
    #[test]
    fn selection_survives_updates_and_removal_has_a_nearby_fallback() {
        let mut list = MobileSessions::default();
        list.update(vec![row(1), row(2), row(3)]);
        list.select(TerminalId(2));
        assert_eq!(list.selected().unwrap().terminal_id, TerminalId(2));
        list.update(vec![row(0), row(1), row(2), row(3)]);
        assert_eq!(list.selected().unwrap().terminal_id, TerminalId(2));
        list.update(vec![row(0), row(1), row(3)]);
        assert_eq!(list.selected().unwrap().terminal_id, TerminalId(3));
        list.update(vec![]);
        assert!(list.selected().is_none());
    }
}
