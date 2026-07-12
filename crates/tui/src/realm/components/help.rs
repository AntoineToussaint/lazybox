//! `Help` — yazi-style which-key panel pinned to the bottom. tuirealm
//! port of `tui_kit::widgets::HelpModal`.
//!
//! A second `?` swaps it for the "Ask lazybox" modal (#302). Arrow /
//! `j` `k` / PgUp / PgDn / Home / End scroll the panel when it overflows
//! a short terminal (#338); any other keyboard event dismisses.

use crate::pane::Binding;
use crate::realm::Msg;
use crate::realm::UserEvent;
use lazybox_tui_core::action::{self, CatalogEntry, Chord, KeyStroke, Section};
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
    /// Section title, rendered as a heading above the section's grid —
    /// the scope that disambiguates a key bound in two sections (#338).
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
    /// The full key display for each in-group action, so any
    /// user-added direct-key aliases stay visible alongside the leader
    /// chord, e.g. `aliases: g m · g v | Shift-V` after an override
    /// (the default keymap ships leader chords only, #304).
    aliases: String,
}

/// Whether this entry's chord is a timed double-press latch (`q q`
/// quit) rather than a catalog leader menu. The live UI dispatches it
/// through the q-latch — pressing `q` never arms a which-key menu (see
/// `keys.rs`'s `opens_leader`) — so the help panel must not render a
/// phantom `q` leader block the user can never open. Keyed on the
/// action's `guard`, so it tracks the dispatch path and doesn't
/// misfire on a real leader whose second key repeats the leader
/// (`g g` auto-merge, a genuine member of the `g` github menu) (#338).
fn is_double_press(entry: &CatalogEntry) -> bool {
    matches!(
        action::ActionDef::for_kind(entry.kind).guard(),
        action::Guard::DoublePress,
    )
}

/// Whether this entry is rendered as part of a leader-group block —
/// i.e. it has a two-step `<leader> <key>` chord AND is dispatched
/// through the catalog leader (not the q-latch). The exact predicate
/// [`LeaderGroup::all_from_catalog`] uses to collect group members,
/// factored out so `from_catalog` can partition the catalog once:
/// leader entries render only in their group, everything else in the
/// flat per-section grid (#338).
fn is_leader_entry(entry: &CatalogEntry) -> bool {
    !is_double_press(entry)
        && entry
            .chords
            .iter()
            .any(|chord| matches!(chord, Chord::Seq(strokes) if strokes.len() == 2))
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
            // The q-latch double-press (`q q`) isn't a menu — skip it so
            // no phantom `q` block appears, matching `is_leader_entry` so
            // the flat/grouped split stays total (#338).
            if is_double_press(entry) {
                continue;
            }
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
                // The group's registry name (`github`, `agent`, …)
                // heads the block — the same label the footer's group
                // cell and the which-key popup title use (#304).
                let label = group
                    .iter()
                    .find_map(|(_, entry)| action::leader_group_label(entry.kind));
                let heading = match label {
                    Some(name) => format!("{name} — press {leader_disp}, then:"),
                    None => format!("press {leader_disp}, then:"),
                };
                Self {
                    heading,
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
    /// Rows scrolled down from the top. Only meaningful when the whole
    /// panel doesn't fit the screen — the leader band plus five section
    /// grids overflow a short terminal, so the bottom (Terminal) rows
    /// would otherwise clip off-screen with no way to reach them. `view`
    /// clamps this to the real overflow each frame (the area height
    /// isn't known until render).
    scroll: u16,
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
            // The catalog is one source of truth for the data, but the
            // render must not show it twice: an entry whose effective
            // chord is a two-step leader sequence (`g m`, `a c`, `q q`)
            // is rendered in its leader-group block up top, so exclude it
            // from the flat per-section grid — which is single-key-only
            // (#338). `is_leader_entry` mirrors exactly what
            // `LeaderGroup::all_from_catalog` picks up.
            if is_leader_entry(entry) {
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
        Self {
            leaders,
            sections,
            scroll: 0,
        }
    }

    /// Total content height in rows: the ask hint, the leader band, its
    /// separator, and every non-empty section (title + grid). Shared by
    /// `view` (to size + clamp scroll) and the scroll tests.
    fn content_rows(&self) -> u16 {
        let leader_rows: u16 = self
            .leaders
            .iter()
            .map(|lg| 1 + lg.chords.len().div_ceil(COLS) as u16 + 1)
            .sum();
        let sep_rows = if self.leaders.is_empty() { 0 } else { 1 };
        let section_rows: u16 = self
            .sections
            .iter()
            .filter(|s| !s.bindings.is_empty())
            .map(|s| 1 + s.bindings.len().div_ceil(COLS) as u16)
            .sum();
        // +1: the "ask lazybox" hint line at the top of the panel.
        1 + leader_rows + sep_rows + section_rows
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

        let sep_rows = if self.leaders.is_empty() { 0 } else { 1 };
        let content_rows = self.content_rows();

        let panel_h = (content_rows + PADDING_Y * 2).min(area.height);
        let panel = Rect {
            x: area.x.saturating_add(PADDING_X.min(area.width)),
            y: area
                .y
                .saturating_add(area.height.saturating_sub(panel_h + 1)),
            width: area.width.saturating_sub(PADDING_X * 2),
            height: panel_h,
        };

        // Vertical scroll: when the panel doesn't fit, the leader band
        // plus five section grids overflow and the bottom rows would clip
        // off-screen unreachable. Reserve the last inner row for a scroll
        // hint, clamp the offset to the real overflow (area height is
        // only known here), and window the content to `view_h` rows.
        let top = panel.y + PADDING_Y;
        let inner_h = panel.height.saturating_sub(PADDING_Y * 2);
        let overflow = content_rows > inner_h;
        let view_h = if overflow {
            inner_h.saturating_sub(1)
        } else {
            inner_h
        };
        let max_scroll = content_rows.saturating_sub(view_h);
        self.scroll = self.scroll.min(max_scroll);
        let scroll = self.scroll;

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

        // Map a 0-based logical content row to its on-screen y, or `None`
        // when it's scrolled out of the `[top, top + view_h)` window.
        let screen_y = |lrow: u16| -> Option<u16> {
            if lrow < scroll {
                return None;
            }
            let sy = top + (lrow - scroll);
            (sy < top + view_h).then_some(sy)
        };

        // Draw a binding grid at logical row `start_lrow`; returns the
        // logical row just past it. Rows scrolled out of the window are
        // skipped. Shared by the leader chords and the section grids.
        let draw_grid = |frame: &mut Frame, items: &[&Binding], start_lrow: u16| -> u16 {
            for (idx, b) in items.iter().enumerate() {
                let lrow = start_lrow + (idx / COLS) as u16;
                let Some(sy) = screen_y(lrow) else { continue };
                let col = cols[idx % COLS];
                let cell = Rect {
                    x: col.x,
                    y: sy,
                    width: col.width,
                    height: 1,
                };
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
            start_lrow + items.len().div_ceil(COLS) as u16
        };

        let full_line = |frame: &mut Frame, text: &str, lrow: u16, style: Style| {
            let Some(sy) = screen_y(lrow) else { return };
            let cell = Rect {
                x: panel.x,
                y: sy,
                width: panel.width,
                height: 1,
            };
            frame.render_widget(
                Paragraph::new(Line::from(Span::styled(format!(" {text}"), style))),
                cell,
            );
        };

        let mut lrow: u16 = 0;
        full_line(
            frame,
            "? ask lazybox anything (searches your keymap, answers in plain language) · any other key closes",
            lrow,
            Style::default()
                .bg(theme.surface)
                .fg(theme.text_dim)
                .add_modifier(Modifier::ITALIC),
        );
        lrow += 1;
        for lg in &self.leaders {
            full_line(
                frame,
                &lg.heading,
                lrow,
                Style::default()
                    .bg(theme.surface)
                    .fg(theme.accent)
                    .add_modifier(Modifier::BOLD),
            );
            lrow += 1;
            let chords: Vec<&Binding> = lg.chords.iter().collect();
            lrow = draw_grid(frame, &chords, lrow);
            full_line(
                frame,
                &lg.aliases,
                lrow,
                Style::default()
                    .bg(theme.surface)
                    .fg(theme.text_dim)
                    .add_modifier(Modifier::ITALIC),
            );
            lrow += 1;
        }
        lrow += sep_rows;

        // Per-section titled grids. Each section's grid is single-key
        // only (leader chords render in the band above), and the title
        // scopes it — so a key bound in two sections (`Space`, `z`,
        // `Enter`) reads unambiguously.
        let section_title_style = Style::default()
            .bg(theme.surface)
            .fg(theme.accent)
            .add_modifier(Modifier::BOLD);
        for section in &self.sections {
            if section.bindings.is_empty() {
                continue;
            }
            full_line(frame, section.title, lrow, section_title_style);
            lrow += 1;
            let items: Vec<&Binding> = section.bindings.iter().collect();
            lrow = draw_grid(frame, &items, lrow);
        }

        // Scroll hint on the reserved bottom row when the panel overflows,
        // so the clipped rows are discoverable and reachable.
        if overflow {
            let above = scroll;
            let below = content_rows.saturating_sub(scroll + view_h);
            let hint = format!("↑/↓ scroll · {above} above · {below} below");
            let cell = Rect {
                x: panel.x,
                y: top + view_h,
                width: panel.width,
                height: 1,
            };
            frame.render_widget(
                Paragraph::new(Line::from(Span::styled(
                    format!(" {hint}"),
                    Style::default()
                        .bg(theme.surface)
                        .fg(theme.accent)
                        .add_modifier(Modifier::ITALIC),
                ))),
                cell,
            );
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
        use tuirealm::event::Key;
        let Event::Keyboard(key) = ev else {
            return None;
        };
        // A second `?` swaps to the ask modal.
        if matches!(key.code, Key::Char('?')) {
            return Some(Msg::HelpAskOpen);
        }
        // Scroll keys navigate the (possibly overflowing) panel instead
        // of dismissing it — vim + arrows + paging, mirroring the ask
        // modal. `view` clamps the offset to the real overflow, so an
        // over-scroll on a panel that fits is a harmless no-op. Any
        // OTHER key still closes, keeping the which-key "press a key to
        // continue" feel.
        match key.code {
            Key::Down | Key::Char('j') => {
                self.scroll = self.scroll.saturating_add(1);
                None
            }
            Key::Up | Key::Char('k') => {
                self.scroll = self.scroll.saturating_sub(1);
                None
            }
            Key::PageDown => {
                self.scroll = self.scroll.saturating_add(10);
                None
            }
            Key::PageUp => {
                self.scroll = self.scroll.saturating_sub(10);
                None
            }
            Key::Home => {
                self.scroll = 0;
                None
            }
            Key::End => {
                self.scroll = u16::MAX;
                None
            }
            _ => Some(Msg::ModalDismissed),
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

    /// Collect every (keys, label) row the help panel renders — flat
    /// per-section grid AND leader-group blocks. The Keys screen is the
    /// union of both halves; a binding shows in exactly one of them.
    fn all_help_rows(help: &Help) -> Vec<(String, String)> {
        let flat = help
            .sections
            .iter()
            .flat_map(|s| s.bindings.iter())
            .map(|b| (b.keys.to_string(), b.label.to_string()));
        let grouped = help
            .leaders
            .iter()
            .flat_map(|lg| lg.chords.iter())
            .map(|b| (b.keys.to_string(), b.label.to_string()));
        flat.chain(grouped).collect()
    }

    /// The generated Keys screen (#102 P4) is the catalog made visible:
    /// it must list the per-agent spawn rows and the long-snooze guard
    /// row — the bindings that only exist in the runtime catalog — with
    /// their effective keys. The per-agent spawns are two-step leader
    /// chords, so they live in the `a` leader block; the single-key
    /// long-snooze lives in the flat grid (#338).
    #[test]
    fn from_catalog_lists_generated_rows() {
        use lazybox_tui_core::action::ActionDef;
        let catalog = ActionDef::catalog(
            &["claude".to_string(), "codex".to_string()],
            &std::collections::BTreeMap::new(),
        );
        let help = Help::from_catalog(&catalog, ']');
        let rows = all_help_rows(&help);
        assert!(
            rows.iter().any(|(k, l)| k == "a c" && l == "spawn claude"),
            "per-agent claude row missing from Keys screen: {rows:?}",
        );
        assert!(
            rows.iter().any(|(k, l)| k == "a x" && l == "spawn codex"),
            "per-agent codex row missing",
        );
        // The single-key long-snooze guard row lives in the flat grid.
        let flat: Vec<(String, String)> = help
            .sections
            .iter()
            .flat_map(|s| s.bindings.iter())
            .map(|b| (b.keys.to_string(), b.label.to_string()))
            .collect();
        assert!(
            flat.iter().any(|(_, l)| l == "long snooze"),
            "long-snooze guard row missing from the flat grid",
        );
        // No bare generic "spawn agent" placeholder survives.
        assert!(!rows.iter().any(|(_, l)| l == "spawn agent"));
    }

    /// A keymap preset's remaps surface in the Keys screen — merge is
    /// the `g m` leader chord (#304), so it renders in the `g` leader
    /// block and NOT in the flat single-key grid (#338).
    #[test]
    fn from_catalog_reflects_preset_overrides() {
        use lazybox_tui_core::action::{ActionDef, keymap_preset};
        let overrides = keymap_preset("vim").unwrap();
        let catalog = ActionDef::catalog(&[], &overrides);
        let help = Help::from_catalog(&catalog, ']');
        let grouped_merge = help
            .leaders
            .iter()
            .flat_map(|lg| lg.chords.iter())
            .find(|b| b.label == "merge PR")
            .map(|b| b.keys.to_string());
        assert_eq!(grouped_merge.as_deref(), Some("g m"));
        // It must not also appear in the flat grid.
        assert!(
            !help
                .sections
                .iter()
                .flat_map(|s| s.bindings.iter())
                .any(|b| b.label == "merge PR"),
            "merge PR leaked into the flat grid — leader chords render once",
        );
    }

    /// The single source of truth in the render: no binding appears in
    /// both a leader-group block and the flat per-section grid, and the
    /// flat grid carries no *leader* chord (a `<leader> <key>` menu). A
    /// same-key double-press (`q q` quit) is not a leader chord and does
    /// belong in the flat grid — it can't open a menu (#338).
    #[test]
    fn no_binding_is_rendered_in_both_halves() {
        use lazybox_tui_core::action::ActionDef;
        let agents = [
            "claude".to_string(),
            "codex".to_string(),
            "cursor".to_string(),
        ];
        let catalog = ActionDef::catalog(&agents, &std::collections::BTreeMap::new());
        let help = Help::from_catalog(&catalog, ']');

        let flat_keys: Vec<String> = help
            .sections
            .iter()
            .flat_map(|s| s.bindings.iter())
            .map(|b| b.keys.to_string())
            .collect();
        // No leader menu (`g m`, `a c`, …) leaks into the flat grid — a
        // multi-token key is only allowed when its two strokes are equal
        // (`q q`) or it's the terminal `]]` chord.
        for keys in &flat_keys {
            let tokens: Vec<&str> = keys.split_whitespace().collect();
            let is_leader_menu =
                tokens.len() == 2 && tokens[0] != tokens[1] && !keys.starts_with(']');
            assert!(
                !is_leader_menu,
                "leader chord {keys:?} leaked into the flat grid",
            );
        }
        // No flat binding is also a leader-group chord.
        let group_keys: std::collections::HashSet<String> = help
            .leaders
            .iter()
            .flat_map(|lg| lg.chords.iter())
            .map(|b| b.keys.to_string())
            .collect();
        for keys in &flat_keys {
            assert!(
                !group_keys.contains(keys),
                "binding {keys:?} is rendered in both a leader block and the flat grid",
            );
        }
        // The double-press quit is a flat Global entry, never a phantom
        // `q` leader block the live UI can't open.
        assert!(
            flat_keys.iter().any(|k| k == "q q"),
            "q q quit must render in the flat grid",
        );
        assert!(
            !group_keys.contains("q q"),
            "q q must not render as a leader block",
        );
        // Sanity: the leader blocks aren't empty — the github group's
        // `g m` and the agent group's `a c` are up there.
        assert!(group_keys.contains("g m"), "expected the g m leader chord");
        assert!(group_keys.contains("a c"), "expected the a c leader chord");
    }

    /// A single-key remap of a normally-leader action (here merge PR,
    /// `g m` → `Shift-M`) is no longer a two-step chord, so it drops out
    /// of the `g` leader block and back into the flat single-key grid —
    /// the binding stays visible, it just moves to the half that matches
    /// its shape (#338).
    #[test]
    fn single_key_remap_of_a_leader_action_lands_in_the_flat_grid() {
        use lazybox_tui_core::action::ActionDef;
        let mut overrides = std::collections::BTreeMap::new();
        overrides.insert("merge_pr".to_string(), "Shift-M".to_string());
        let catalog = ActionDef::catalog(&[], &overrides);
        let help = Help::from_catalog(&catalog, ']');

        assert!(
            help.sections
                .iter()
                .flat_map(|s| s.bindings.iter())
                .any(|b| b.keys == "Shift-M" && b.label == "merge PR"),
            "a single-key merge remap must show in the flat grid",
        );
        // …and it must NOT also render in a leader block.
        assert!(
            !help
                .leaders
                .iter()
                .flat_map(|lg| lg.chords.iter())
                .any(|b| b.label == "merge PR"),
            "single-key merge remap must not appear in a leader block",
        );
    }

    /// Repo-group collapse (`Space`) is a catalog action now (#338), so
    /// it renders in the flat Sidebar grid of the Keys screen — the
    /// shortcut that used to be an out-of-catalog pane arm and never
    /// showed in help.
    #[test]
    fn from_catalog_lists_repo_group_collapse() {
        use lazybox_tui_core::action::ActionDef;
        let catalog = ActionDef::catalog(&[], &std::collections::BTreeMap::new());
        let help = Help::from_catalog(&catalog, ']');
        let flat: Vec<(String, String)> = help
            .sections
            .iter()
            .flat_map(|s| s.bindings.iter())
            .map(|b| (b.keys.to_string(), b.label.to_string()))
            .collect();
        assert!(
            flat.iter()
                .any(|(k, l)| k == "Space" && l == "collapse group"),
            "repo-group collapse missing from the Keys screen: {flat:?}",
        );
    }

    /// Scroll keys move the offset; a non-scroll key still dismisses and
    /// a second `?` opens the ask modal (#338 follow-up).
    #[test]
    fn scroll_keys_drive_the_offset() {
        use crate::realm::Msg;
        use lazybox_tui_core::action::ActionDef;
        use tuirealm::component::AppComponent;
        use tuirealm::event::{Event, Key, KeyEvent, KeyModifiers};
        let catalog = ActionDef::catalog(&[], &std::collections::BTreeMap::new());
        let mut help = Help::from_catalog(&catalog, ']');
        let press = |code| Event::Keyboard(KeyEvent::new(code, KeyModifiers::NONE));

        assert_eq!(help.scroll, 0);
        assert!(help.on(&press(Key::Down)).is_none());
        assert!(help.on(&press(Key::Char('j'))).is_none());
        assert_eq!(help.scroll, 2);
        assert!(help.on(&press(Key::Up)).is_none());
        assert_eq!(help.scroll, 1);
        // Up saturates at 0.
        help.on(&press(Key::Up));
        help.on(&press(Key::Up));
        assert_eq!(help.scroll, 0);
        // End jumps to the max sentinel (view clamps it); Home resets.
        help.on(&press(Key::End));
        assert_eq!(help.scroll, u16::MAX);
        help.on(&press(Key::Home));
        assert_eq!(help.scroll, 0);
        // `?` swaps to ask; any non-scroll key dismisses.
        assert!(matches!(
            help.on(&press(Key::Char('?'))),
            Some(Msg::HelpAskOpen),
        ));
        assert!(matches!(
            help.on(&press(Key::Esc)),
            Some(Msg::ModalDismissed),
        ));
    }

    /// On a short terminal the panel overflows: the scroll hint appears
    /// and the bottom (Terminal) section is initially clipped but becomes
    /// reachable by scrolling. On a tall terminal everything fits, so no
    /// hint shows and the offset stays pinned at 0 (#338 follow-up).
    #[test]
    fn overflow_scrolls_to_reveal_the_bottom_section() {
        use lazybox_tui_core::action::ActionDef;
        use tuirealm::component::Component;
        use tuirealm::ratatui::Terminal;
        use tuirealm::ratatui::backend::TestBackend;

        let catalog = ActionDef::catalog(&[], &std::collections::BTreeMap::new());
        let render = |help: &mut Help, w: u16, h: u16| -> String {
            let mut term = Terminal::new(TestBackend::new(w, h)).unwrap();
            term.draw(|f| {
                let area = f.area();
                help.view(f, area);
            })
            .unwrap();
            format!("{:?}", term.backend().buffer())
        };

        // Tall: the whole panel fits — no scroll hint, bottom row shown,
        // and an over-scroll is clamped back to 0.
        let mut tall = Help::from_catalog(&catalog, ']');
        tall.scroll = 50;
        let out = render(&mut tall, 110, 60);
        assert!(
            out.contains("exit to sidebar"),
            "Terminal row must show when it fits"
        );
        assert!(
            !out.contains("below"),
            "no scroll hint when everything fits"
        );
        assert_eq!(tall.scroll, 0, "over-scroll clamps to 0 when it all fits");

        // Short: overflows. At the top the hint shows and the Terminal
        // row is clipped; scrolling to the end reveals it.
        let mut short = Help::from_catalog(&catalog, ']');
        let top = render(&mut short, 110, 16);
        assert!(top.contains("below"), "overflow must show the scroll hint");
        assert!(
            top.contains("ask lazybox"),
            "top of the panel is visible at scroll 0"
        );
        assert!(
            !top.contains("exit to sidebar"),
            "bottom is clipped at scroll 0"
        );

        short.scroll = u16::MAX; // End
        let bottom = render(&mut short, 110, 16);
        assert!(
            bottom.contains("exit to sidebar"),
            "scrolling reveals the Terminal row"
        );
        assert!(bottom.contains("above"), "hint shows rows scrolled above");
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
            // `g g` repeats the leader key but is a real menu member
            // (auto-merge), NOT a q-latch double-press — it must stay in
            // the github block, not fall through to the flat grid (#338).
            ("g g", "auto-merge"),
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

    /// The default keymap is leaders-only (#304): no legacy Shift-*
    /// alias may reappear on the g group's aliases line, and the
    /// heading carries the group's registry name so the help panel,
    /// footer cell, and which-key popup all tell the same story.
    #[test]
    fn leader_section_is_alias_free_and_carries_the_group_label() {
        use lazybox_tui_core::action::ActionDef;
        let catalog = ActionDef::catalog(&[], &std::collections::BTreeMap::new());
        let help = Help::from_catalog(&catalog, ']');
        let g = help
            .leaders
            .iter()
            .find(|lg| lg.heading.contains("press g"))
            .expect("g leader block missing from help panel");
        assert!(
            g.heading.starts_with("github"),
            "g group heading missing its registry label: {:?}",
            g.heading,
        );
        for alias in ["Shift-M", "Shift-V", "Shift-G", "Shift-L", "Shift-O"] {
            assert!(
                !g.aliases.contains(alias),
                "legacy alias {alias} leaked back into the aliases line: {:?}",
                g.aliases,
            );
        }
    }

    /// The `a` agent group forms its own leader block, labeled from the
    /// registry, listing every generated spawn chord (#304).
    #[test]
    fn leader_section_lists_the_agent_chords_from_the_catalog() {
        use lazybox_tui_core::action::ActionDef;
        let agents = [
            "claude".to_string(),
            "codex".to_string(),
            "cursor".to_string(),
        ];
        let catalog = ActionDef::catalog(&agents, &std::collections::BTreeMap::new());
        let help = Help::from_catalog(&catalog, ']');
        let a = help
            .leaders
            .iter()
            .find(|lg| lg.heading.contains("press a"))
            .expect("a leader block missing from help panel");
        assert!(
            a.heading.starts_with("agent"),
            "a group heading missing its registry label: {:?}",
            a.heading,
        );
        let rows: Vec<(String, String)> = a
            .chords
            .iter()
            .map(|b| (b.keys.to_string(), b.label.to_string()))
            .collect();
        for (chord, label) in [
            ("a c", "spawn claude"),
            ("a x", "spawn codex"),
            ("a u", "spawn cursor"),
        ] {
            assert!(
                rows.iter().any(|(k, l)| k == chord && l == label),
                "a leader block missing {chord} → {label}; got {rows:?}",
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
