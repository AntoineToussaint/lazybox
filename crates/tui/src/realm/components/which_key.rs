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

use lazybox_tui_core::action::{ActionDef, ActionGroup};
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

/// Render the which-key nudge shown after the FIRST press of the
/// `q q` quit chord (#100). Unlike the leader popups this isn't a
/// binding set — the chord is a time-windowed double-tap — so it's a
/// single instructional line: complete the chord, or cancel. Styled
/// and anchored to match [`render`] so the affordance reads the same
/// as the `g`-leader popup the issue points to.
///
/// `quit_keys` is the effective binding display (default `"q q"`);
/// the first token is the key to press again.
pub fn render_quit_hint(frame: &mut Frame, area: Rect, quit_keys: &str) {
    let theme = crate::theme::current();
    let press = quit_keys.split_whitespace().next().unwrap_or("q");

    let panel_h = 4u16.min(area.height);
    // Wider than PANEL_W: the instruction line is a full sentence.
    let panel_w = 34u16.min(area.width);
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

    let title = Line::from(Span::styled(
        " quit ",
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
    let line = Line::from(vec![
        Span::styled(format!("  {press}"), key_style),
        Span::styled(" again to quit · ", label_style),
        Span::styled("Esc", key_style),
        Span::styled(" cancel", label_style),
    ]);
    frame.render_widget(
        Paragraph::new(line),
        Rect {
            x: panel.x,
            y: panel.y + 2,
            width: panel.width,
            height: 1,
        },
    );
}

/// Largest snippet list the terminal-leader popup will enumerate
/// before collapsing the tail into a "+N more" row. Keeps the popup
/// from swallowing the screen on a big library.
const LEADER_MAX_ROWS: usize = 8;

/// Render the which-key popup for the armed terminal `]]` leader
/// (issue #205). Lists the agent-jump roster (`]]<digit>` → workspace,
/// `]]f` → focus mode) on top of the snippet keys reachable as
/// `]]<key>`, plus a hint that an idle window leaves the pane. Visual
/// twin of [`render`], but the binding set is the agent roster + the
/// snippet library, so it takes the rows directly.
pub fn render_terminal_leader(
    frame: &mut Frame,
    area: Rect,
    escape_char: char,
    agents: &[(String, String)],
    snippets: &[(String, String)],
) {
    let theme = crate::theme::current();
    // Agent jumps come first — they're why a heads-down user armed the
    // leader — then the snippet library. One flat list, capped together.
    let rows: Vec<(String, String)> = agents.iter().chain(snippets.iter()).cloned().collect();
    let shown = rows.len().min(LEADER_MAX_ROWS);
    let overflow = rows.len() - shown;
    let extra_rows = if overflow > 0 { 1 } else { 0 };
    // title + footer hint + one row per shown binding (+ overflow),
    // plus a blank row top and bottom.
    let panel_h = (shown as u16 + extra_rows as u16 + 4).min(area.height);
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

    let title_kind = if snippets.is_empty() {
        "jump"
    } else {
        "jump · snippets"
    };
    let title = Line::from(Span::styled(
        format!(" {escape_char}{escape_char} · {title_kind} "),
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
    for (i, (k, desc)) in rows.iter().take(shown).enumerate() {
        let y = panel.y + 2 + i as u16;
        if y >= panel.y + panel.height {
            break;
        }
        let line = Line::from(vec![
            Span::styled(format!("  {k}"), key_style),
            Span::styled("  ", bg),
            Span::styled(desc.clone(), label_style),
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
    if overflow > 0 {
        let y = panel.y + 2 + shown as u16;
        if y < panel.y + panel.height {
            frame.render_widget(
                Paragraph::new(Line::from(Span::styled(
                    format!("  +{overflow} more…"),
                    Style::default().bg(theme.surface).fg(theme.text_dim),
                ))),
                Rect {
                    x: panel.x,
                    y,
                    width: panel.width,
                    height: 1,
                },
            );
        }
    }
    // Footer hint on the bottom row of the panel.
    let hint_y = panel.y + panel.height.saturating_sub(1);
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            " Esc cancel · idle leaves ",
            Style::default().bg(theme.surface).fg(theme.text_dim),
        ))),
        Rect {
            x: panel.x,
            y: hint_y,
            width: panel.width,
            height: 1,
        },
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use tuirealm::ratatui::Terminal;
    use tuirealm::ratatui::backend::TestBackend;

    fn render_to_string(quit_keys: &str) -> String {
        let (w, h) = (80u16, 24u16);
        let backend = TestBackend::new(w, h);
        let mut term = Terminal::new(backend).unwrap();
        term.draw(|f| {
            render_quit_hint(f, Rect::new(0, 0, w, h), quit_keys);
        })
        .unwrap();
        let buf = term.backend().buffer().clone();
        (0..h)
            .map(|y| (0..w).map(|x| buf[(x, y)].symbol()).collect::<String>())
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn quit_hint_names_the_press_key_and_cancel() {
        let out = render_to_string("q q");
        assert!(out.contains("quit"), "missing title");
        assert!(out.contains("q again to quit"), "missing press-again nudge");
        assert!(out.contains("Esc cancel"), "missing cancel affordance");
    }

    #[test]
    fn quit_hint_uses_first_token_of_remapped_chord() {
        // A user who remapped quit to `x x` should be told to press x.
        let out = render_to_string("x x");
        assert!(out.contains("x again to quit"));
    }
}
