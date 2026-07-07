//! `Help` — yazi-style which-key panel pinned to the bottom. tuirealm
//! port of `tui_kit::widgets::HelpModal`.
//!
//! Any keyboard event dismisses.

use crate::pane::Binding;
use crate::realm::Msg;
use crate::realm::UserEvent;
use lazybox_tui_core::action::{CatalogEntry, Chord, KeyStroke, Section};
use tuirealm::command::{Cmd, CmdResult};
use tuirealm::component::{AppComponent, Component};
use tuirealm::event::Event;
use tuirealm::props::{AttrValue, Attribute, QueryResult};
use tuirealm::ratatui::Frame;
use tuirealm::ratatui::layout::{Constraint, Layout, Rect};
use tuirealm::ratatui::prelude::*;
use tuirealm::ratatui::widgets::{Block, Clear, Paragraph};
use tuirealm::state::State;

/// One section of the help panel — title + bindings under it.
pub struct HelpSection {
    /// Section title (rendered today flatten into one grid; reserved
    /// for future per-section styling).
    pub title: &'static str,
    /// Bindings for this section. Owned so we can carry user-override
    /// keys without leaking a `&'static` slice per render.
    pub bindings: Vec<Binding>,
}

/// A leader-key chord group rendered as its own labeled block at the
/// top of the panel — the which-key system surfaced as discoverable
/// UI so users learn that `g` *opens a menu* rather than memorizing
/// five separate chords. Derived from the runtime catalog (every entry
/// whose effective chord is a two-step `<leader> <key>` sequence), the
/// data-driven replacement for the old `ActionGroup` table so the
/// leader, chords, and aliases can't drift from the live keymap
/// (issues #145, #102).
pub struct LeaderGroup {
    /// Heading line, e.g. `press g, then:`.
    heading: String,
    /// One row per in-group chord: keys `g m`, label `merge PR`.
    chords: Vec<Binding>,
    /// The full key display for each in-group action, so the legacy
    /// direct-key aliases (`Shift-V`, …) stay visible alongside the
    /// leader chord, e.g. `aliases: g m · g v | Shift-V`.
    aliases: String,
}

impl LeaderGroup {
    /// Build one block per distinct leader keystroke that begins a
    /// two-step chord in the catalog, in first-appearance order. Each
    /// block lists every continuation's `<leader> <key>` display and
    /// its catalog label. Mirrors the live which-key popup, which keys
    /// off the same `Chord::Seq` data.
    fn all_from_catalog(catalog: &[CatalogEntry]) -> Vec<Self> {
        // First-appearance order of leaders, with their continuations.
        let mut leaders: Vec<KeyStroke> = Vec::new();
        let mut members: Vec<Vec<(KeyStroke, &CatalogEntry)>> = Vec::new();
        for entry in catalog {
            for chord in &entry.chords {
                let Chord::Seq(strokes) = chord else { continue };
                if strokes.len() != 2 {
                    continue;
                }
                let leader = strokes[0];
                let idx = leaders
                    .iter()
                    .position(|l| *l == leader)
                    .unwrap_or_else(|| {
                        leaders.push(leader);
                        members.push(Vec::new());
                        leaders.len() - 1
                    });
                members[idx].push((strokes[1], entry));
            }
        }
        leaders
            .iter()
            .zip(members.iter())
            .map(|(leader, group)| {
                let leader_disp = leader.display();
                let chords = group
                    .iter()
                    .map(|(second, entry)| Binding {
                        keys: std::borrow::Cow::Owned(format!(
                            "{leader_disp} {}",
                            second.display()
                        )),
                        label: entry.label.clone(),
                    })
                    .collect();
                let aliases = group
                    .iter()
                    .map(|(_, entry)| entry.keys_display.as_ref())
                    .collect::<Vec<_>>()
                    .join(" · ");
                Self {
                    heading: format!("press {leader_disp}, then:"),
                    chords,
                    aliases: format!("aliases: {aliases}"),
                }
            })
            .collect()
    }
}

/// Yazi-style which-key panel.
pub struct Help {
    leaders: Vec<LeaderGroup>,
    sections: Vec<HelpSection>,
}

impl Help {
    /// Build the help panel from the runtime catalog — every action,
    /// including the generated per-agent `SpawnAgent` rows, with
    /// effective (post-override) chords. The user sees a complete
    /// reference instead of the pane-stitched subset the legacy
    /// constructor produced.
    ///
    /// `escape_char` is the configured `ui.terminal_escape_char`: the
    /// `]]` leave chord and `]]<key>` snippet leader are dispatched by
    /// the terminal escape-char latch, not the catalog, so their display
    /// is rendered from the char doubled here rather than the catalog's
    /// hardcoded `]]` default — otherwise a user who remaps the escape
    /// char sees `}}` in the footer but `]]` in `?` help (#188).
    pub fn from_catalog(catalog: &[CatalogEntry], escape_char: char) -> Self {
        use lazybox_tui_core::action::ActionKind;
        let leader = format!("{escape_char}{escape_char}");
        let mut by_section: std::collections::BTreeMap<u8, (&'static str, Vec<Binding>)> =
            std::collections::BTreeMap::new();
        for entry in catalog {
            // An agent with no default binding and no remap has nothing
            // to show in the keys column — skip it rather than render a
            // blank row.
            if entry.keys_display.is_empty() {
                continue;
            }
            // LeaveTerminal's chord is the escape char doubled, owned by
            // the terminal latch — render it from the live char, not the
            // catalog default / a `leave_terminal` override the dispatch
            // ignores (#188).
            let keys = if entry.kind == ActionKind::LeaveTerminal {
                std::borrow::Cow::Owned(leader.clone())
            } else {
                entry.keys_display.clone()
            };
            by_section
                .entry(entry.section.order())
                .or_insert_with(|| (entry.section.title(), Vec::new()))
                .1
                .push(Binding {
                    keys,
                    label: entry.label.clone(),
                });
        }
        // The snippet leader (`]]<key>`) isn't a catalog `Action` —
        // it's a terminal-pane chord whose binding set is the user's
        // snippet library — so it's hand-added to the Terminal section
        // here, the same way the hint bar curates it (issue #205).
        by_section
            .entry(Section::Terminal.order())
            .or_insert_with(|| (Section::Terminal.title(), Vec::new()))
            .1
            .push(Binding {
                keys: std::borrow::Cow::Owned(format!("{leader}<key>")),
                label: std::borrow::Cow::Borrowed("snippets"),
            });
        let sections: Vec<HelpSection> = by_section
            .into_iter()
            .map(|(_, (title, bindings))| HelpSection { title, bindings })
            .collect();
        let leaders = LeaderGroup::all_from_catalog(catalog);
        Self { leaders, sections }
    }

    fn flat(&self) -> Vec<&Binding> {
        self.sections
            .iter()
            .flat_map(|s| s.bindings.iter())
            .collect()
    }
}

const COLS: usize = 3;
const PADDING_Y: u16 = 1;
const PADDING_X: u16 = 1;

impl Component for Help {
    fn view(&mut self, frame: &mut Frame, area: Rect) {
        let theme = crate::theme::current();
        let bindings = self.flat();
        if bindings.is_empty() {
            return;
        }

        // The leader band is a heading + a chord grid + an aliases note
        // per group, with one blank separator before the catalog grid.
        let leader_rows: u16 = self
            .leaders
            .iter()
            .map(|lg| 1 + lg.chords.len().div_ceil(COLS) as u16 + 1)
            .sum();
        let sep_rows = if self.leaders.is_empty() { 0 } else { 1 };
        let grid_rows = bindings.len().div_ceil(COLS) as u16;
        let content_rows = leader_rows + sep_rows + grid_rows;

        let panel_h = (content_rows + PADDING_Y * 2).min(area.height);
        let panel = Rect {
            x: area.x.saturating_add(PADDING_X.min(area.width)),
            y: area
                .y
                .saturating_add(area.height.saturating_sub(panel_h + 1)),
            width: area.width.saturating_sub(PADDING_X * 2),
            height: panel_h,
        };

        if panel.y > area.y {
            let mask = Rect {
                x: area.x,
                y: area.y,
                width: area.width,
                height: panel.y - area.y,
            };
            frame.render_widget(
                Block::default().style(Style::default().bg(Color::Black)),
                mask,
            );
        }

        frame.render_widget(Clear, panel);
        let panel_bg = Style::default().bg(theme.surface);
        frame.render_widget(Block::default().style(panel_bg), panel);

        let col_constraints: Vec<Constraint> = (0..COLS)
            .map(|_| Constraint::Ratio(1, COLS as u32))
            .collect();
        let cols = Layout::horizontal(col_constraints).split(panel);
        let bottom = panel.y + panel.height;

        // Draw a binding grid starting at `start_y`; returns the row
        // just past it. Shared by the leader chords and the catalog.
        let draw_grid = |frame: &mut Frame, items: &[&Binding], start_y: u16| -> u16 {
            for (idx, b) in items.iter().enumerate() {
                let col = cols[idx % COLS];
                let cell = Rect {
                    x: col.x,
                    y: start_y + (idx / COLS) as u16,
                    width: col.width,
                    height: 1,
                };
                if cell.y >= bottom {
                    break;
                }
                const KEY_PAD: usize = 14;
                let mut key = format_keys_for_display(&b.keys);
                if key.chars().count() < KEY_PAD {
                    key.push_str(&" ".repeat(KEY_PAD - key.chars().count()));
                }
                let key_style = Style::default()
                    .bg(theme.surface)
                    .fg(theme.accent)
                    .add_modifier(Modifier::BOLD);
                let sep_style = Style::default().bg(theme.surface).fg(theme.text_dim);
                let label_style = Style::default().bg(theme.surface).fg(theme.text_strong);
                let line = Line::from(vec![
                    Span::styled(" ", panel_bg),
                    Span::styled(key, key_style),
                    Span::styled("  ", sep_style),
                    Span::styled(b.label.clone(), label_style),
                ]);
                frame.render_widget(Paragraph::new(line), cell);
            }
            start_y + items.len().div_ceil(COLS) as u16
        };

        let full_line = |frame: &mut Frame, text: &str, y: u16, style: Style| {
            if y >= bottom {
                return;
            }
            let cell = Rect {
                x: panel.x,
                y,
                width: panel.width,
                height: 1,
            };
            frame.render_widget(
                Paragraph::new(Line::from(Span::styled(format!(" {text}"), style))),
                cell,
            );
        };

        let mut y = panel.y + PADDING_Y;
        for lg in &self.leaders {
            full_line(
                frame,
                &lg.heading,
                y,
                Style::default()
                    .bg(theme.surface)
                    .fg(theme.accent)
                    .add_modifier(Modifier::BOLD),
            );
            y += 1;
            let chords: Vec<&Binding> = lg.chords.iter().collect();
            y = draw_grid(frame, &chords, y);
            full_line(
                frame,
                &lg.aliases,
                y,
                Style::default()
                    .bg(theme.surface)
                    .fg(theme.text_dim)
                    .add_modifier(Modifier::ITALIC),
            );
            y += 1;
        }
        y += sep_rows;

        draw_grid(frame, &bindings, y);
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

impl AppComponent<Msg, UserEvent> for Help {
    fn on(&mut self, ev: &Event<UserEvent>) -> Option<Msg> {
        if matches!(ev, Event::Keyboard(_)) {
            Some(Msg::ModalDismissed)
        } else {
            None
        }
    }
}

/// Normalize a `Binding`'s raw key string for the help panel.
///
/// Source bindings drifted to a mix of conventions over time —
/// `Shift-M` (Title case after modifier) vs `Ctrl-c` (lowercase
/// after modifier) vs bare `X` (no modifier syntax at all). The user
/// flagged this as "the shortcuts are a mess." Instead of rewriting
/// every site, we normalize at render time:
///
/// - `Modifier-letter`: emit `Modifier+LETTER` (always uppercase
///   the letter, `+` separator so it doesn't visually collide with
///   the `g/G` dual-binding form).
/// - `Ctrl-Shift-letter`: emit `Ctrl+Shift+LETTER`.
/// - Anything else (bare letters, `q q`, `↑/↓`, `Tab`, `?`): leaves
///   it alone — those are already in a consistent shape.
fn format_keys_for_display(raw: &str) -> String {
    // Order matters: check the longest prefix first.
    const PREFIXES: &[(&str, &str)] = &[
        ("Ctrl-Shift-", "Ctrl+Shift"),
        ("Ctrl-", "Ctrl"),
        ("Shift-", "Shift"),
        ("Alt-", "Alt"),
        ("Cmd-", "Cmd"),
    ];
    for (prefix, normalized) in PREFIXES {
        if let Some(rest) = raw.strip_prefix(prefix) {
            // Uppercase the rest IF it's a single ASCII letter.
            // Words like `Arrows`, `PgUp/Dn` keep their natural
            // casing.
            let rest_norm = if rest.chars().count() == 1
                && rest.chars().next().is_some_and(|c| c.is_ascii_alphabetic())
            {
                rest.to_ascii_uppercase()
            } else {
                rest.to_string()
            };
            return format!("{normalized}+{rest_norm}");
        }
    }
    raw.to_string()
}

#[cfg(test)]
mod tests {
    use super::{Help, format_keys_for_display};

    /// The generated Keys screen (#102 P4) is the catalog made visible:
    /// it must list the per-agent spawn rows and the long-snooze guard
    /// row — the bindings that only exist in the runtime catalog — with
    /// their effective keys, grouped by scope.
    #[test]
    fn from_catalog_lists_generated_rows() {
        use lazybox_tui_core::action::ActionDef;
        let catalog = ActionDef::catalog(
            &["claude".to_string(), "codex".to_string()],
            &std::collections::BTreeMap::new(),
        );
        let help = Help::from_catalog(&catalog, ']');
        let rows: Vec<(String, String)> = help
            .sections
            .iter()
            .flat_map(|s| s.bindings.iter())
            .map(|b| (b.keys.to_string(), b.label.to_string()))
            .collect();
        assert!(
            rows.iter().any(|(k, l)| k == "c" && l == "spawn claude"),
            "per-agent claude row missing from Keys screen: {rows:?}",
        );
        assert!(
            rows.iter().any(|(k, l)| k == "x" && l == "spawn codex"),
            "per-agent codex row missing",
        );
        assert!(
            rows.iter().any(|(_, l)| l == "long snooze"),
            "long-snooze guard row missing",
        );
        // No bare generic "spawn agent" placeholder survives.
        assert!(!rows.iter().any(|(_, l)| l == "spawn agent"));
    }

    /// A keymap preset's remaps surface in the Keys screen — the vim
    /// preset shows merge as `g m`, not `Shift-M`.
    #[test]
    fn from_catalog_reflects_preset_overrides() {
        use lazybox_tui_core::action::{ActionDef, keymap_preset};
        let overrides = keymap_preset("vim").unwrap();
        let catalog = ActionDef::catalog(&[], &overrides);
        let help = Help::from_catalog(&catalog, ']');
        let merge_keys = help
            .sections
            .iter()
            .flat_map(|s| s.bindings.iter())
            .find(|b| b.label == "merge PR")
            .map(|b| b.keys.to_string());
        assert_eq!(merge_keys.as_deref(), Some("g m"));
    }

    /// The `g` leader gets its own labeled block built from the catalog:
    /// every in-group chord (`g m`, …) with its catalog label, so the
    /// which-key section can't drift from the live keymap (#145, #114).
    #[test]
    fn leader_section_lists_the_github_chords_from_the_catalog() {
        use lazybox_tui_core::action::ActionDef;
        let catalog = ActionDef::catalog(&[], &std::collections::BTreeMap::new());
        let help = Help::from_catalog(&catalog, ']');
        let g = help
            .leaders
            .iter()
            .find(|lg| lg.heading.contains("press g"))
            .expect("g leader block missing from help panel");
        let rows: Vec<(String, String)> = g
            .chords
            .iter()
            .map(|b| (b.keys.to_string(), b.label.to_string()))
            .collect();
        for (chord, label) in [
            ("g m", "merge PR"),
            ("g v", "reviewers"),
            ("g a", "assignees"),
            ("g l", "labels"),
            ("g o", "open in browser"),
        ] {
            assert!(
                rows.iter().any(|(k, l)| k == chord && l == label),
                "g leader block missing {chord} → {label}; got {rows:?}",
            );
        }
    }

    /// The scoped `w c` / `w x` / `w u` work chords (#224) form their
    /// own `w` leader block in the help panel, built from the catalog
    /// exactly like the `g` github group — so they surface in the
    /// which-key popup the same way.
    #[test]
    fn leader_section_lists_the_scoped_work_chords_from_the_catalog() {
        use lazybox_tui_core::action::ActionDef;
        let agents = [
            "claude".to_string(),
            "codex".to_string(),
            "cursor".to_string(),
        ];
        let catalog = ActionDef::catalog(&agents, &std::collections::BTreeMap::new());
        let help = Help::from_catalog(&catalog, ']');
        let w = help
            .leaders
            .iter()
            .find(|lg| lg.heading.contains("press w"))
            .expect("w leader block missing from help panel");
        let rows: Vec<(String, String)> = w
            .chords
            .iter()
            .map(|b| (b.keys.to_string(), b.label.to_string()))
            .collect();
        for (chord, label) in [
            ("w c", "work in claude"),
            ("w x", "work in codex"),
            ("w u", "work in cursor"),
        ] {
            assert!(
                rows.iter().any(|(k, l)| k == chord && l == label),
                "w leader block missing {chord} → {label}; got {rows:?}",
            );
        }
    }

    /// The aliases line carries each action's full key display, so the
    /// legacy Shift-* aliases stay visible alongside the leader chord —
    /// derived from the same catalog key displays.
    #[test]
    fn leader_section_reflects_the_shift_aliases() {
        use lazybox_tui_core::action::ActionDef;
        let catalog = ActionDef::catalog(&[], &std::collections::BTreeMap::new());
        let help = Help::from_catalog(&catalog, ']');
        let g = help
            .leaders
            .iter()
            .find(|lg| lg.heading.contains("press g"))
            .expect("g leader block missing from help panel");
        for alias in ["Shift-V", "Shift-G", "Shift-L", "Shift-O"] {
            assert!(
                g.aliases.contains(alias),
                "aliases line omits {alias}: {:?}",
                g.aliases,
            );
        }
    }

    #[test]
    fn modifier_letter_normalized() {
        assert_eq!(format_keys_for_display("Shift-M"), "Shift+M");
        assert_eq!(format_keys_for_display("Ctrl-c"), "Ctrl+C");
        assert_eq!(format_keys_for_display("Ctrl-Shift-D"), "Ctrl+Shift+D");
    }

    #[test]
    fn modifier_named_key_preserves_casing() {
        assert_eq!(format_keys_for_display("Shift-Arrows"), "Shift+Arrows");
        assert_eq!(format_keys_for_display("Shift-PgUp/Dn"), "Shift+PgUp/Dn");
    }

    #[test]
    fn unmodified_keys_pass_through() {
        assert_eq!(format_keys_for_display("r"), "r");
        assert_eq!(format_keys_for_display("?"), "?");
        assert_eq!(format_keys_for_display("Tab"), "Tab");
        assert_eq!(format_keys_for_display("q q"), "q q");
        assert_eq!(format_keys_for_display("g/G"), "g/G");
        assert_eq!(format_keys_for_display("↑/↓"), "↑/↓");
    }
}
