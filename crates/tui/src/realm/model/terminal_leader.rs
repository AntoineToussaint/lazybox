//! Single source of truth for the terminal `]]` leader's fixed command
//! menu (#205, #252, #286).
//!
//! One table drives BOTH the armed-leader key dispatch
//! (`keys::handle_pane_key`) and the which-key popup rows
//! (`Model::view`), so the two can't drift: a command added here shows
//! up in the popup and resolves on the keyboard, and the unit tests
//! below fail if a menu row stops mapping to a command.

use lazybox_core::TileDirection;
use tuirealm::event::{Key, KeyModifiers};

/// A resolved `]]<key>` command. The Model maps each variant onto its
/// handler; `None` from [`LeaderCmd::from_key`] means "not a command —
/// cancel back into the terminal" (the tmux-prefix convention).
pub(super) enum LeaderCmd {
    /// `]]<1..9>` — jump to the Nth agent workspace (sidebar order).
    JumpAgent(usize),
    /// `]]s` — open the snippet picker.
    Snippets,
    /// `]]f` — toggle focus mode.
    ToggleFocusMode,
    /// `]]q` — exit the terminal back to the sidebar.
    ExitToSidebar,
    /// `` ]]` `` — open the fuzzy jump-to-workspace picker.
    JumpPicker,
    /// `]]|` (or `]]\`) — split the focused tile side-by-side.
    SplitVertical,
    /// `]]-` — split the focused tile stacked.
    SplitHorizontal,
    /// `]]<arrow>` — move tile focus (cycles tabs in Tabs mode).
    MoveTile(TileDirection),
    /// `]]x` — close the focused terminal (the focused tile in Splits,
    /// the active tab in Tabs).
    CloseTerminal,
}

impl LeaderCmd {
    /// Resolve a keystroke arriving while the leader is armed.
    ///
    /// Letters and digits require an unmodified press — a shifted
    /// letter is a different character and must stay a cancel, so a
    /// future lowercase command never silently gains a shifted alias.
    /// Symbol chords additionally accept SHIFT because `|` (and `-` on
    /// some layouts) arrives with the modifier set on most hosts.
    pub(super) fn from_key(code: Key, modifiers: KeyModifiers) -> Option<Self> {
        if let Key::Char(c) = code
            && !c.is_control()
        {
            let shifted_symbol = !c.is_alphanumeric() && modifiers == KeyModifiers::SHIFT;
            if !(modifiers.is_empty() || shifted_symbol) {
                return None;
            }
            return match c {
                '1'..='9' => Some(Self::JumpAgent(c.to_digit(10)? as usize)),
                's' => Some(Self::Snippets),
                'f' => Some(Self::ToggleFocusMode),
                'q' => Some(Self::ExitToSidebar),
                '`' => Some(Self::JumpPicker),
                '|' | '\\' => Some(Self::SplitVertical),
                '-' => Some(Self::SplitHorizontal),
                'x' => Some(Self::CloseTerminal),
                _ => None,
            };
        }
        if !modifiers.is_empty() {
            return None;
        }
        match code {
            Key::Left => Some(Self::MoveTile(TileDirection::Left)),
            Key::Right => Some(Self::MoveTile(TileDirection::Right)),
            Key::Up => Some(Self::MoveTile(TileDirection::Up)),
            Key::Down => Some(Self::MoveTile(TileDirection::Down)),
            _ => None,
        }
    }

    /// The fixed rows of the which-key popup, ordered head-first so
    /// truncation ("+N more") can only ever eat the agent-jump roster
    /// the caller appends. Rows are tailored to the active layout so
    /// the popup never advertises a chord that would be a no-op:
    /// `move tile` only exists in Splits, `switch tab` only with two
    /// or more tabs.
    ///
    /// INVARIANT: each row's key column is *literally* the dispatch
    /// character (`s`, `|`, `x`, …), never a decorative glyph — this is
    /// what lets `Enter` on a highlighted row re-derive its command by
    /// feeding that char back through [`Self::from_key`] (#343). The one
    /// multi-char entry, the arrow aggregate, is deliberately
    /// non-dispatchable (it needs a direction) and is skipped there.
    pub(super) fn menu_rows(splits: bool, tab_count: usize) -> Vec<(String, String)> {
        let mut rows: Vec<(&str, &str)> = vec![
            ("s", "snippets"),
            ("f", "focus mode"),
            ("q", "exit to sidebar"),
            ("`", "jump to workspace"),
            ("|", "split right"),
            ("-", "split down"),
        ];
        if splits {
            rows.push(("←↓↑→", "move tile"));
        } else if tab_count >= 2 {
            rows.push(("←→", "switch tab"));
        }
        rows.push(("x", "close terminal"));
        rows.into_iter()
            .map(|(k, l)| (k.to_string(), l.to_string()))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every key the popup advertises must resolve to a command —
    /// this is the anti-drift contract between `menu_rows` and
    /// `from_key`. Arrow rows are display aggregates (`←↓↑→`), so
    /// they're vouched for by the dedicated arrow test below.
    #[test]
    fn every_menu_row_key_resolves_to_a_command() {
        for (key, label) in LeaderCmd::menu_rows(true, 1) {
            if key.contains('←') {
                continue;
            }
            let mut chars = key.chars();
            let (c, rest) = (chars.next().unwrap(), chars.next());
            assert!(rest.is_none(), "menu key `{key}` should be a single char");
            assert!(
                LeaderCmd::from_key(Key::Char(c), KeyModifiers::NONE).is_some(),
                "menu row `{key}` / `{label}` maps to no command",
            );
        }
    }

    #[test]
    fn arrows_resolve_to_tile_moves_only_unmodified() {
        for (key, dir) in [
            (Key::Left, TileDirection::Left),
            (Key::Right, TileDirection::Right),
            (Key::Up, TileDirection::Up),
            (Key::Down, TileDirection::Down),
        ] {
            match LeaderCmd::from_key(key, KeyModifiers::NONE) {
                Some(LeaderCmd::MoveTile(d)) => assert_eq!(d, dir),
                _ => panic!("{key:?} must resolve to MoveTile"),
            }
            assert!(
                LeaderCmd::from_key(key, KeyModifiers::SHIFT).is_none(),
                "modified arrows are not leader commands",
            );
        }
    }

    /// `|` reaches us with SHIFT set on most hosts; letters must NOT
    /// get the same leniency (`]]S` is a cancel, not snippets).
    #[test]
    fn shift_is_accepted_for_symbols_but_not_letters() {
        assert!(matches!(
            LeaderCmd::from_key(Key::Char('|'), KeyModifiers::SHIFT),
            Some(LeaderCmd::SplitVertical)
        ));
        assert!(LeaderCmd::from_key(Key::Char('S'), KeyModifiers::SHIFT).is_none());
        assert!(LeaderCmd::from_key(Key::Char('X'), KeyModifiers::SHIFT).is_none());
    }

    /// The popup only advertises what the current layout can honor.
    #[test]
    fn menu_rows_are_tailored_to_the_layout() {
        let labels = |rows: Vec<(String, String)>| -> Vec<String> {
            rows.into_iter().map(|(_, l)| l).collect()
        };
        let splits = labels(LeaderCmd::menu_rows(true, 2));
        assert!(splits.iter().any(|l| l == "move tile"));
        assert!(!splits.iter().any(|l| l == "switch tab"));

        let one_tab = labels(LeaderCmd::menu_rows(false, 1));
        assert!(!one_tab.iter().any(|l| l == "move tile"));
        assert!(!one_tab.iter().any(|l| l == "switch tab"));
        assert!(one_tab.iter().any(|l| l == "close terminal"));

        let two_tabs = labels(LeaderCmd::menu_rows(false, 2));
        assert!(two_tabs.iter().any(|l| l == "switch tab"));
    }
}
