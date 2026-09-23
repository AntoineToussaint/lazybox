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
    // Presentation preference for this client; never reorders daemon terminals.
    order: Vec<lazybox_config::MobileSessionTab>,
}

impl MobileSessions {
    pub(crate) fn update(&mut self, mut rows: Vec<SessionRow>) {
        let old_position = self.position();
        // The daemon streams its roster during attachment. Do not discard
        // saved identities just because their spawn event has not arrived yet.
        if !self.order.is_empty() {
            rows.sort_by_key(|row| {
                self.order
                    .iter()
                    .position(|tab| {
                        tab.terminal_id == row.terminal_id.0
                            && tab.session_key == row.session_key.as_str()
                    })
                    .unwrap_or(usize::MAX)
            });
        }
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

    /// Move into the target's position, shifting intervening rows. Both IDs
    /// come from the painted list; a vanished destination must not redirect it.
    pub(crate) fn prioritize(&mut self, source: TerminalId, target: TerminalId) -> bool {
        let Some(from) = self.rows.iter().position(|r| r.terminal_id == source) else {
            return false;
        };
        let Some(to) = self.rows.iter().position(|r| r.terminal_id == target) else {
            return false;
        };
        let row = self.rows.remove(from);
        self.rows.insert(to, row);
        self.order = self
            .rows
            .iter()
            .map(|row| lazybox_config::MobileSessionTab {
                session_key: row.session_key.as_str().to_owned(),
                terminal_id: row.terminal_id.0,
            })
            .collect();
        true
    }

    pub(crate) fn restore_order(&mut self, order: Vec<lazybox_config::MobileSessionTab>) {
        self.order = order;
        let rows = std::mem::take(&mut self.rows);
        self.update(rows);
    }

    pub(crate) fn saved_order(&self) -> Vec<lazybox_config::MobileSessionTab> {
        self.order.clone()
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
    pub(super) fn row(id: u64) -> SessionRow {
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

#[cfg(test)]
mod priority_tests {
    use super::{tests::row, *};
    #[test]
    fn saved_priority_survives_empty_and_partial_rosters_and_checks_workspace() {
        let mut list = MobileSessions::default();
        list.restore_order(vec![
            lazybox_config::MobileSessionTab {
                session_key: "session-3".into(),
                terminal_id: 3,
            },
            lazybox_config::MobileSessionTab {
                session_key: "session-1".into(),
                terminal_id: 1,
            },
        ]);
        list.update(vec![]);
        list.update(vec![row(1)]);
        list.update(vec![row(1), row(2), row(3)]);
        assert_eq!(
            list.rows
                .iter()
                .map(|r| r.terminal_id.0)
                .collect::<Vec<_>>(),
            [3, 1, 2]
        );
        let mut reused = row(3);
        reused.session_key = SessionKey::new("another-workspace");
        list.update(vec![reused, row(1)]);
        assert_eq!(list.rows[0].terminal_id, TerminalId(1));
    }

    #[test]
    fn priority_inserts_in_both_directions_and_survives_roster_refresh() {
        let mut list = MobileSessions::default();
        list.update(vec![row(1), row(2), row(3)]);
        list.select(TerminalId(2));
        assert!(list.prioritize(TerminalId(3), TerminalId(1)));
        list.update(vec![row(1), row(2), row(3), row(4)]);
        assert_eq!(
            list.rows
                .iter()
                .map(|r| r.terminal_id.0)
                .collect::<Vec<_>>(),
            [3, 1, 2, 4]
        );
        assert_eq!(list.selected().unwrap().terminal_id, TerminalId(2));
        assert!(list.prioritize(TerminalId(3), TerminalId(2)));
        assert_eq!(
            list.rows
                .iter()
                .map(|r| r.terminal_id.0)
                .collect::<Vec<_>>(),
            [1, 2, 3, 4]
        );
        assert!(!list.prioritize(TerminalId(9), TerminalId(2)));
        assert!(!list.prioritize(TerminalId(2), TerminalId(9)));
        list.update(vec![row(4), row(2)]);
        assert_eq!(
            list.rows
                .iter()
                .map(|r| r.terminal_id.0)
                .collect::<Vec<_>>(),
            [2, 4]
        );
    }
}
