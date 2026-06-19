//! `Help` — yazi-style which-key panel pinned to the bottom. tuirealm
//! port of `tui_kit::widgets::HelpModal`.
//!
//! Any keyboard event dismisses.

use crate::pane::Binding;
use crate::realm::Msg;
use crate::realm::UserEvent;
use lazybox_tui_core::action::{ActionDef, ActionGroup, Section};
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
/// five separate chords. Built from [`ActionGroup`] so the leader,
/// chords, and aliases all track the catalog (issue #145).
pub struct LeaderGroup {
    /// Heading line, e.g. `github — press g, then:`.
    heading: String,
    /// One row per in-group chord: keys `g m`, label `merge PR`.
    chords: Vec<Binding>,
    /// The legacy direct-key aliases this group replaced, e.g.
    /// `aliases: Shift-M · Shift-V · Shift-G · Shift-L · Shift-O`.
    aliases: String,
}

impl LeaderGroup {
    fn from_action_group(group: ActionGroup) -> Self {
        let chords = group
            .members()
            .iter()
            .map(|(key, kind)| Binding {
                keys: std::borrow::Cow::Owned(format!("{} {key}", group.leader())),
                label: std::borrow::Cow::Borrowed(ActionDef::for_kind(*kind).label),
            })
            .collect();
        let aliases = group
            .members()
            .iter()
            .map(|(_, kind)| ActionDef::for_kind(*kind).default_keys)
            .collect::<Vec<_>>()
            .join(" · ");
        Self {
            heading: format!("{} — press {}, then:", group.title(), group.leader()),
            chords,
            aliases: format!("aliases: {aliases}"),
        }
    }
}

/// Yazi-style which-key panel.
pub struct Help {
    leaders: Vec<LeaderGroup>,
    sections: Vec<HelpSection>,
}

impl Help {
    /// Build the help panel from `ActionDef::all()` — the canonical
    /// catalog — honoring user keybinding overrides. Every action
    /// surfaces; the user sees a complete reference instead of the
    /// pane-stitched subset the legacy constructor produced.
    pub fn from_catalog(overrides: &std::collections::BTreeMap<String, String>) -> Self {
        let mut by_section: std::collections::BTreeMap<u8, Vec<Binding>> =
            std::collections::BTreeMap::new();
        for def in ActionDef::all() {
            let order = match def.section {
                Section::Global => 0,
                Section::Workspace => 1,
                Section::Activity => 2,
                Section::Terminal => 3,
            };
            by_section.entry(order).or_default().push(Binding {
                keys: def.effective_keys_display(overrides),
                label: std::borrow::Cow::Borrowed(def.label),
            });
        }
        // The snippet leader (`]]<key>`) isn't a catalog `Action` —
        // it's a terminal-pane chord whose binding set is the user's
        // snippet library — so it's hand-added to the Terminal section
        // here, the same way the hint bar curates it (issue #205).
        by_section.entry(3).or_default().push(Binding {
            keys: std::borrow::Cow::Borrowed("]]<key>"),
            label: std::borrow::Cow::Borrowed("snippets"),
        });
        let sections: Vec<HelpSection> = by_section
            .into_iter()
            .map(|(order, bindings)| {
                let title = match order {
                    0 => "Global",
                    1 => "Workspace",
                    2 => "Activity",
                    _ => "Terminal",
                };
                HelpSection { title, bindings }
            })
            .collect();
        let leaders = ActionGroup::all()
            .iter()
            .map(|g| LeaderGroup::from_action_group(*g))
            .collect();
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
    use lazybox_tui_core::action::{ActionDef, ActionGroup};

    /// Render the help panel into a throwaway backend and return the
    /// visible text — the surface for asserting the leader section.
    fn render_help() -> String {
        use tuirealm::component::Component;
        use tuirealm::ratatui::Terminal;
        use tuirealm::ratatui::backend::TestBackend;
        use tuirealm::ratatui::layout::Rect;
        let mut h = Help::from_catalog(&std::collections::BTreeMap::new());
        let (w, ht) = (120u16, 40u16);
        let mut term = Terminal::new(TestBackend::new(w, ht)).unwrap();
        term.draw(|f| h.view(f, Rect::new(0, 0, w, ht))).unwrap();
        let buf = term.backend().buffer();
        (0..buf.area.height)
            .map(|y| {
                let mut row = String::new();
                for x in 0..buf.area.width {
                    row.push_str(buf[(x, y)].symbol());
                }
                row.trim_end().to_string()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn leader_section_lists_the_github_chords_from_the_catalog() {
        // The g leader gets its own labeled block: the group title +
        // every in-group chord and its catalog label. Pulling from the
        // catalog keeps it from drifting (issue #145, #114).
        let text = render_help();
        let group = ActionGroup::Github;
        assert!(text.contains(group.title()), "leader section title missing");
        for (key, kind) in group.members() {
            let chord = format!("{} {key}", group.leader());
            assert!(text.contains(&chord), "help missing leader chord {chord}");
            let label = ActionDef::for_kind(*kind).label;
            assert!(text.contains(label), "help missing label {label:?}");
        }
    }

    #[test]
    fn leader_section_reflects_the_shift_aliases() {
        // Each chord's legacy Shift-* key still works; the section says
        // so, derived from the same catalog defaults.
        let text = render_help();
        for (_, kind) in ActionGroup::Github.members() {
            let alias = ActionDef::for_kind(*kind).default_keys;
            assert!(
                text.contains(alias),
                "help leader section omits alias {alias}"
            );
        }
        assert!(text.contains("aliases"), "aliases note missing");
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
