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

/// One priority move, replayable onto any copy of the saved order.
#[derive(Clone, Debug)]
pub(crate) struct PriorityMove {
    pub(crate) source: lazybox_config::MobileSessionTab,
    pub(crate) target: lazybox_config::MobileSessionTab,
    /// The roster as painted, in display order, so a replay onto a stale
    /// on-disk order still knows which tabs exist and where they sit.
    pub(crate) live: Vec<lazybox_config::MobileSessionTab>,
}

/// Apply one priority move to a saved order, as an operation on the value
/// passed in rather than an assignment of a snapshot.
///
/// Live tabs missing from `order` are appended in display order, so a new
/// terminal follows the saved ones. The source then moves into the index the
/// target occupied, matching what the painted list showed. Saved tabs whose
/// terminal has not arrived yet are never removed — that is the whole point:
/// `Config::save_with_async` runs against the config as freshly loaded from
/// disk, so writing an in-memory snapshot there discards whatever another
/// client, or this client's own not-yet-streamed roster, had persisted
/// (see `Config::mutate_ui_list`, #1244).
pub(crate) fn reorder(
    order: &mut Vec<lazybox_config::MobileSessionTab>,
    live: &[lazybox_config::MobileSessionTab],
    source: &lazybox_config::MobileSessionTab,
    target: &lazybox_config::MobileSessionTab,
) -> bool {
    for tab in live {
        if !order.contains(tab) {
            order.push(tab.clone());
        }
    }
    let Some(from) = order.iter().position(|tab| tab == source) else {
        return false;
    };
    // `to` is read before the removal, so a downward move lands after the
    // target and an upward move before it — the positions the letters showed.
    let Some(to) = order.iter().position(|tab| tab == target) else {
        return false;
    };
    let tab = order.remove(from);
    order.insert(to.min(order.len()), tab);
    true
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

    fn tab_for(&self, id: TerminalId) -> Option<lazybox_config::MobileSessionTab> {
        self.rows
            .iter()
            .find(|r| r.terminal_id == id)
            .map(|row| lazybox_config::MobileSessionTab {
                session_key: row.session_key.as_str().to_owned(),
                terminal_id: row.terminal_id.0,
            })
    }

    /// Move into the target's position, shifting intervening rows. Both IDs
    /// come from the painted list; a vanished destination must not redirect it.
    ///
    /// Returns the move as data so the caller can replay it onto the config as
    /// it is on disk. The old code instead rebuilt the saved order from the
    /// visible rows, which silently deleted the priority of every tab the
    /// daemon had not streamed in yet (#1877 review B4).
    pub(crate) fn prioritize(
        &mut self,
        source: TerminalId,
        target: TerminalId,
    ) -> Option<PriorityMove> {
        let source = self.tab_for(source)?;
        let target = self.tab_for(target)?;
        let live: Vec<_> = self
            .rows
            .iter()
            .filter_map(|r| self.tab_for(r.terminal_id))
            .collect();
        if !reorder(&mut self.order, &live, &source, &target) {
            return None;
        }
        // The display follows the saved order, so there is one ordering rule.
        let rows = std::mem::take(&mut self.rows);
        self.update(rows);
        Some(PriorityMove {
            source,
            target,
            live,
        })
    }

    pub(crate) fn restore_order(&mut self, order: Vec<lazybox_config::MobileSessionTab>) {
        self.order = order;
        let rows = std::mem::take(&mut self.rows);
        self.update(rows);
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
    fn reordering_a_partial_roster_keeps_the_priority_of_tabs_still_streaming_in() {
        // On attach the daemon streams its roster one terminal at a time.
        // Reordering in that window used to rewrite the saved order from the
        // visible rows alone, deleting the priority of every tab that had not
        // arrived — including ones the user never touched.
        let saved = |keys: &[(&str, u64)]| -> Vec<lazybox_config::MobileSessionTab> {
            keys.iter()
                .map(|(k, id)| lazybox_config::MobileSessionTab {
                    session_key: (*k).into(),
                    terminal_id: *id,
                })
                .collect()
        };
        let mut list = MobileSessions::default();
        let on_disk = saved(&[("session-7", 7), ("session-8", 8), ("session-9", 9)]);
        list.restore_order(on_disk.clone());
        list.update(vec![row(7), row(8)]); // 9 has not been streamed yet
        let moved = list
            .prioritize(TerminalId(8), TerminalId(7))
            .expect("both rows are painted");
        // Replaying onto the order as it is on disk is what the model asks
        // `Config::save_with_async` to do.
        let mut disk = on_disk;
        assert!(reorder(
            &mut disk,
            &moved.live,
            &moved.source,
            &moved.target
        ));
        assert_eq!(
            disk.iter().map(|t| t.terminal_id).collect::<Vec<_>>(),
            [8, 7, 9],
            "the unarrived tab keeps its saved place"
        );
        // And once it does arrive it lands where it was saved.
        let mut fresh = MobileSessions::default();
        fresh.restore_order(disk);
        fresh.update(vec![row(7), row(8), row(9)]);
        assert_eq!(
            fresh
                .rows()
                .iter()
                .map(|r| r.terminal_id.0)
                .collect::<Vec<_>>(),
            [8, 7, 9]
        );
    }

    #[test]
    fn a_replay_onto_a_stale_order_does_not_drop_a_sibling_clients_tabs() {
        // Two mobile clients share one config profile. The second one's move
        // must not erase rows the first one persisted.
        let mut disk = vec![
            lazybox_config::MobileSessionTab {
                session_key: "from-the-other-client".into(),
                terminal_id: 42,
            },
            lazybox_config::MobileSessionTab {
                session_key: "session-1".into(),
                terminal_id: 1,
            },
        ];
        let mut list = MobileSessions::default();
        list.update(vec![row(1), row(2)]);
        let moved = list.prioritize(TerminalId(2), TerminalId(1)).unwrap();
        assert!(reorder(
            &mut disk,
            &moved.live,
            &moved.source,
            &moved.target
        ));
        assert!(
            disk.iter().any(|t| t.terminal_id == 42),
            "{disk:?} lost the sibling's tab"
        );
    }

    #[test]
    fn priority_inserts_in_both_directions_and_survives_roster_refresh() {
        let mut list = MobileSessions::default();
        list.update(vec![row(1), row(2), row(3)]);
        list.select(TerminalId(2));
        assert!(list.prioritize(TerminalId(3), TerminalId(1)).is_some());
        list.update(vec![row(1), row(2), row(3), row(4)]);
        assert_eq!(
            list.rows
                .iter()
                .map(|r| r.terminal_id.0)
                .collect::<Vec<_>>(),
            [3, 1, 2, 4]
        );
        assert_eq!(list.selected().unwrap().terminal_id, TerminalId(2));
        assert!(list.prioritize(TerminalId(3), TerminalId(2)).is_some());
        assert_eq!(
            list.rows
                .iter()
                .map(|r| r.terminal_id.0)
                .collect::<Vec<_>>(),
            [1, 2, 3, 4]
        );
        assert!(list.prioritize(TerminalId(9), TerminalId(2)).is_none());
        assert!(list.prioritize(TerminalId(2), TerminalId(9)).is_none());
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
