//! `Help` — yazi-style which-key panel pinned to the bottom. tuirealm
//! port of `tui_kit::widgets::HelpModal`.
//!
//! Any keyboard event dismisses.

use crate::pane::Binding;
use crate::realm::Msg;
use crate::realm::UserEvent;
use lazybox_tui_core::action::{CatalogEntry, Section};
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

/// Yazi-style which-key panel.
pub struct Help {
    sections: Vec<HelpSection>,
}

impl Help {
    /// Build the help panel from the runtime catalog — every action,
    /// including the generated per-agent `SpawnAgent` rows, with
    /// effective (post-override) chords. The user sees a complete
    /// reference instead of the pane-stitched subset the legacy
    /// constructor produced.
    pub fn from_catalog(catalog: &[CatalogEntry]) -> Self {
        let mut by_section: std::collections::BTreeMap<u8, (&'static str, Vec<Binding>)> =
            std::collections::BTreeMap::new();
        for entry in catalog {
            // An agent with no default binding and no remap has nothing
            // to show in the keys column — skip it rather than render a
            // blank row.
            if entry.keys_display.is_empty() {
                continue;
            }
            by_section
                .entry(entry.section.order())
                .or_insert_with(|| (entry.section.title(), Vec::new()))
                .1
                .push(Binding {
                    keys: entry.keys_display.clone(),
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
                keys: std::borrow::Cow::Borrowed("]]<key>"),
                label: std::borrow::Cow::Borrowed("snippets"),
            });
        let sections: Vec<HelpSection> = by_section
            .into_iter()
            .map(|(_, (title, bindings))| HelpSection { title, bindings })
            .collect();
        Self { sections }
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
        let rows = bindings.len().div_ceil(COLS) as u16;
        let panel_h = (rows + PADDING_Y * 2).min(area.height);
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

        for (idx, b) in bindings.iter().enumerate() {
            let col_idx = idx % COLS;
            let row_idx = (idx / COLS) as u16;
            let col = cols[col_idx];
            let cell = Rect {
                x: col.x,
                y: col.y + PADDING_Y + row_idx,
                width: col.width,
                height: 1,
            };
            if cell.y >= panel.y + panel.height {
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
    use super::format_keys_for_display;

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
