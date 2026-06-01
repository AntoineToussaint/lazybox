//! `which_key` — popup for the grouped two-step (leader-key) chords
//! from issue #126.
//!
//! Rendered inline by `Model::view` (NOT a modal) while a group leader
//! is armed: the leader latch owns the keyboard via `handle_pane_key`,
//! so this is pure chrome. It lists the armed group's actions and the
//! second key that fires each — the discoverability win the issue asks
//! for ("pressing the leader can show a which-key-style popup listing
//! the group's actions").
//!
//! Styled to match the yazi-style `help` panel: a `surface`-filled
//! box anchored bottom-left, just above the footer.

use pilot_tui_core::action::{ActionDef, ActionGroup};
use tuirealm::ratatui::Frame;
use tuirealm::ratatui::layout::Rect;
use tuirealm::ratatui::prelude::*;
use tuirealm::ratatui::widgets::{Block, Clear, Paragraph};

/// Popup width in cells. Wide enough for `key  longest-label` without
/// wrapping; clamped to the frame width on a narrow terminal.
const PANEL_W: u16 = 28;

/// Render the which-key popup for `group`. `area` is the full frame;
/// the popup floats in the bottom-left corner, one row above the
/// footer so it doesn't cover the hint bar.
pub fn render(frame: &mut Frame, area: Rect, group: ActionGroup) {
    let theme = crate::theme::current();
    let members = group.members();
    // One title row + one row per member, plus a blank row top and
    // bottom for breathing room.
    let panel_h = (members.len() as u16 + 3).min(area.height);
    let panel_w = PANEL_W.min(area.width);
    let panel = Rect {
        x: area.x,
        y: area
            .y
            .saturating_add(area.height.saturating_sub(panel_h + 1)),
        width: panel_w,
        height: panel_h,
    };

    let bg = Style::default().bg(theme.surface);
    frame.render_widget(Clear, panel);
    frame.render_widget(Block::default().style(bg), panel);

    // Title: "<leader> · <group>" so the user sees which chord is in
    // flight (e.g. "g · github").
    let title = Line::from(Span::styled(
        format!(" {} · {} ", group.leader(), group.title()),
        Style::default()
            .bg(theme.surface)
            .fg(theme.text_dim)
            .add_modifier(Modifier::BOLD),
    ));
    frame.render_widget(
        Paragraph::new(title),
        Rect {
            x: panel.x,
            y: panel.y + 1,
            width: panel.width,
            height: 1,
        },
    );

    let key_style = Style::default()
        .bg(theme.surface)
        .fg(theme.accent)
        .add_modifier(Modifier::BOLD);
    let label_style = Style::default().bg(theme.surface).fg(theme.text_strong);
    for (i, (k, kind)) in members.iter().enumerate() {
        let y = panel.y + 2 + i as u16;
        if y >= panel.y + panel.height {
            break;
        }
        let label = ActionDef::for_kind(*kind).label;
        let line = Line::from(vec![
            Span::styled(format!("  {k}"), key_style),
            Span::styled("  ", bg),
            Span::styled(label, label_style),
        ]);
        frame.render_widget(
            Paragraph::new(line),
            Rect {
                x: panel.x,
                y,
                width: panel.width,
                height: 1,
            },
        );
    }
}
