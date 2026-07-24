//! Single source of truth for the terminal `]]` leader's fixed command
//! menu (#205, #252, #286).
//!
//! One table drives BOTH the armed-leader key dispatch
//! (`keys::handle_pane_key`) and the which-key popup rows
//! (`Model::view`), so the two can't drift: a command added here shows
//! up in the popup and resolves on the keyboard, and the unit tests
//! below fail if a menu row stops mapping to a command.

use lazybox_config::NewTerminalLayout;
use lazybox_core::TileDirection;
use tuirealm::event::{Key, KeyModifiers};

/// A resolved `]]<key>` command. The Model maps each variant onto its
/// handler; `None` from [`LeaderCmd::from_key`] means "not a command —
/// cancel back into the terminal" (the tmux-prefix convention).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum LeaderCmd {
    /// `]]<1..9>` — jump to the Nth agent workspace (sidebar order).
    JumpAgent(usize),
    /// `]]s` — open the snippet picker.
    Snippets,
    /// `]]r` — recall the last prompt (the in-flight draft, else the
    /// last submitted message) back into the agent's composer, without
    /// submitting it, so a restart doesn't lose what you'd typed.
    RecallPrompt,
    /// `]]h` — open the per-session prompt-history picker (issue #523):
    /// every prompt sent to this agent, newest-first, snippet entries
    /// tagged; Enter re-sends the picked one.
    PromptHistory,
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
    /// `]]t` — flip the new-terminal layout preference (split ⇄ tabs)
    /// and persist it. Affects the *next* spawn, not open terminals.
    ToggleNewLayout,
}

/// One fixed character command. Dispatch, the runtime which-key popup,
/// and the generated website reference all read this table; adding a
/// command in only one of those surfaces is therefore impossible.
#[derive(Clone, Copy)]
struct FixedCommandSpec {
    key: char,
    command: LeaderCmd,
    menu_label: &'static str,
    reference: &'static str,
}

const FIXED_COMMANDS: &[FixedCommandSpec] = &[
    FixedCommandSpec {
        key: 's',
        command: LeaderCmd::Snippets,
        menu_label: "snippets",
        reference: "Open the snippet picker (typing a full key auto-submits — `]]srev`)",
    },
    FixedCommandSpec {
        key: 'r',
        command: LeaderCmd::RecallPrompt,
        menu_label: "recall prompt",
        reference: "Restore the in-flight draft, or the last submitted agent prompt, without sending it",
    },
    FixedCommandSpec {
        key: 'h',
        command: LeaderCmd::PromptHistory,
        menu_label: "prompt history",
        reference: "Browse this session's prompt history (newest-first, snippets tagged); Enter re-sends one",
    },
    FixedCommandSpec {
        key: 'f',
        command: LeaderCmd::ToggleFocusMode,
        menu_label: "focus mode",
        reference: "Toggle focus mode",
    },
    FixedCommandSpec {
        key: 'q',
        command: LeaderCmd::ExitToSidebar,
        menu_label: "exit to sidebar",
        reference: "Exit to the sidebar",
    },
    FixedCommandSpec {
        key: '`',
        command: LeaderCmd::JumpPicker,
        menu_label: "jump to workspace",
        reference: "Open the fuzzy jump-to-workspace picker",
    },
    FixedCommandSpec {
        key: '|',
        command: LeaderCmd::SplitVertical,
        menu_label: "split right",
        reference: "Split the focused tile side-by-side (`]]\\` is an alias)",
    },
    FixedCommandSpec {
        key: '-',
        command: LeaderCmd::SplitHorizontal,
        menu_label: "split down",
        reference: "Split the focused tile stacked",
    },
    FixedCommandSpec {
        key: 'x',
        command: LeaderCmd::CloseTerminal,
        menu_label: "close terminal",
        reference: "Close the focused terminal (tile or active tab)",
    },
    FixedCommandSpec {
        key: 't',
        command: LeaderCmd::ToggleNewLayout,
        menu_label: "new shells",
        reference: "Toggle whether the next terminal opens as a split or a tab; persists `ui.terminal_new_layout`",
    },
];

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
            if let '1'..='9' = c {
                return Some(Self::JumpAgent(c.to_digit(10)? as usize));
            }
            // `\\` is the easy-to-type alias for `|` on layouts where
            // the shifted symbol is awkward; both resolve through the
            // same canonical table row.
            let canonical = if c == '\\' { '|' } else { c };
            return FIXED_COMMANDS
                .iter()
                .find(|spec| spec.key == canonical)
                .map(|spec| spec.command);
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
    pub(super) fn menu_rows(
        splits: bool,
        tab_count: usize,
        new_layout: NewTerminalLayout,
    ) -> Vec<(String, String)> {
        let mut rows = Vec::with_capacity(FIXED_COMMANDS.len() + 1);
        for spec in FIXED_COMMANDS {
            // Tile/tab navigation sits next to the split controls and
            // before close, matching the established menu order.
            if spec.key == 'x' {
                if splits {
                    rows.push(("←↓↑→".to_string(), "move tile".to_string()));
                } else if tab_count >= 2 {
                    rows.push(("←→".to_string(), "switch tab".to_string()));
                }
            }
            let label = if spec.key == 't' {
                // Show the current default so this doubles as a status row.
                match new_layout {
                    NewTerminalLayout::Split => "new shells: split",
                    NewTerminalLayout::Tabs => "new shells: tabs",
                }
            } else {
                spec.menu_label
            };
            rows.push((spec.key.to_string(), label.to_string()));
        }
        rows
    }

    /// Complete, layout-independent command list for generated docs.
    /// Fixed character rows come from [`FIXED_COMMANDS`], the same table
    /// dispatch and the popup consume; dynamic digit/arrow families are
    /// included explicitly because they carry runtime operands.
    pub(super) fn reference_rows() -> Vec<(String, String)> {
        let mut rows = Vec::with_capacity(FIXED_COMMANDS.len() + 2);
        for spec in FIXED_COMMANDS {
            rows.push((spec.key.to_string(), spec.reference.to_string()));
            if spec.key == '`' {
                rows.push((
                    "1…9".to_string(),
                    "Jump to the Nth agent workspace (sidebar order)".to_string(),
                ));
            }
            if spec.key == '-' {
                rows.push((
                    "←↓↑→".to_string(),
                    "Move tile focus; Left/Right cycles tabs in Tabs mode".to_string(),
                ));
            }
        }
        rows
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
        for (key, label) in LeaderCmd::menu_rows(true, 1, NewTerminalLayout::Split) {
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

    /// The website reference is runtime-backed: every fixed row resolves
    /// through the same dispatcher, including commands added after the
    /// original hand-written appendix (`r` and `t`).
    #[test]
    fn every_fixed_reference_row_resolves_to_a_command() {
        let rows = LeaderCmd::reference_rows();
        for (key, label) in rows.iter().filter(|(key, _)| key.chars().count() == 1) {
            let key = key.chars().next().expect("one-character key");
            assert!(
                LeaderCmd::from_key(Key::Char(key), KeyModifiers::NONE).is_some(),
                "reference row `{key}` / `{label}` maps to no command",
            );
        }
        assert!(rows.iter().any(|(key, _)| key == "r"));
        assert!(rows.iter().any(|(key, _)| key == "t"));
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
        let splits = labels(LeaderCmd::menu_rows(true, 2, NewTerminalLayout::Split));
        assert!(splits.iter().any(|l| l == "move tile"));
        assert!(!splits.iter().any(|l| l == "switch tab"));

        let one_tab = labels(LeaderCmd::menu_rows(false, 1, NewTerminalLayout::Split));
        assert!(!one_tab.iter().any(|l| l == "move tile"));
        assert!(!one_tab.iter().any(|l| l == "switch tab"));
        assert!(one_tab.iter().any(|l| l == "close terminal"));

        let two_tabs = labels(LeaderCmd::menu_rows(false, 2, NewTerminalLayout::Split));
        assert!(two_tabs.iter().any(|l| l == "switch tab"));
    }

    /// The `]]t` row always shows and reflects the current default;
    /// its key resolves like every other command row.
    #[test]
    fn layout_toggle_row_reflects_current_preference() {
        let split = LeaderCmd::menu_rows(false, 1, NewTerminalLayout::Split);
        assert!(
            split
                .iter()
                .any(|(k, l)| k == "t" && l == "new shells: split")
        );

        let tabs = LeaderCmd::menu_rows(false, 1, NewTerminalLayout::Tabs);
        assert!(
            tabs.iter()
                .any(|(k, l)| k == "t" && l == "new shells: tabs")
        );

        assert!(matches!(
            LeaderCmd::from_key(Key::Char('t'), KeyModifiers::NONE),
            Some(LeaderCmd::ToggleNewLayout)
        ));
    }
}
