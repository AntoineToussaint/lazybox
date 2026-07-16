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

use lazybox_tui_core::action::KeyStroke;
use tuirealm::ratatui::Frame;
use tuirealm::ratatui::layout::Rect;
use tuirealm::ratatui::prelude::*;
use tuirealm::ratatui::widgets::{Block, Clear, Paragraph};

/// Popup width in cells. Wide enough for `key  longest-label` without
/// wrapping; clamped to the frame width on a narrow terminal.
const PANEL_W: u16 = 28;

/// Draw one `key  label` menu row into `rect`, styled selected when it
/// carries the popup highlight (arrow / `j`/`k` navigation, #343): a
/// `fill` background across the row plus a `▸` caret, matching the
/// snippet picker's cursor row. Unselected rows keep the flat `surface`
/// look. Shared by the catalog and terminal-leader popups so their
/// highlight reads identically.
fn render_menu_row(
    frame: &mut Frame,
    rect: Rect,
    key: &str,
    label: &str,
    selected: bool,
    theme: &crate::theme::Theme,
) {
    let row_bg = if selected { theme.fill } else { theme.surface };
    if selected {
        frame.render_widget(Block::default().style(Style::default().bg(row_bg)), rect);
    }
    let mut key_style = Style::default()
        .bg(row_bg)
        .fg(theme.accent)
        .add_modifier(Modifier::BOLD);
    let mut label_style = Style::default().bg(row_bg).fg(theme.text_strong);
    if selected {
        label_style = label_style.add_modifier(Modifier::BOLD);
        key_style = key_style.fg(theme.text_strong);
    }
    let caret = if selected { "▸ " } else { "  " };
    let line = Line::from(vec![
        Span::styled(format!("{caret}{key}"), key_style),
        Span::styled("  ", Style::default().bg(row_bg)),
        Span::styled(label.to_string(), label_style),
    ]);
    frame.render_widget(Paragraph::new(line), rect);
}

/// Render the which-key popup for an armed leader `prefix`. `rows` are
/// the `(next-key-display, label)` continuations of that prefix — a
/// pure function of the catalog supplied by the caller, no longer a
/// hardcoded group table (#102). `group` is the leader group's name
/// (`github`, `agent`, …) when the continuations belong to one — it
/// titles the popup with the same label the footer's group cell
/// advertises (#304). `area` is the full frame; the popup floats in
/// the bottom-left corner, one row above the footer so it doesn't
/// cover the hint bar.
///
/// `highlight` is the row the arrow / `j` / `k` keys have moved the
/// selection to (`None` in the direct-letter default); it's drawn with
/// a `fill`-backed caret so `Enter` has a visible target (#343).
pub fn render(
    frame: &mut Frame,
    area: Rect,
    prefix: KeyStroke,
    group: Option<&str>,
    rows: &[(String, String)],
    highlight: Option<usize>,
) {
    let theme = crate::theme::current();
    // One title row + one row per continuation, plus a blank row top
    // and bottom for breathing room.
    let panel_h = (rows.len() as u16 + 3).min(area.height);
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

    // Title: the armed prefix (plus its group's name when it has one)
    // so the user sees which chord is in flight (e.g. "g ▸ github").
    let title_text = match group {
        Some(g) => format!(" {} ▸ {g} ", prefix.display()),
        None => format!(" {} … ", prefix.display()),
    };
    let title = Line::from(Span::styled(
        title_text,
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

    for (i, (k, label)) in rows.iter().enumerate() {
        let y = panel.y + 2 + i as u16;
        if y >= panel.y + panel.height {
            break;
        }
        render_menu_row(
            frame,
            Rect {
                x: panel.x,
                y,
                width: panel.width,
                height: 1,
            },
            k,
            label,
            highlight == Some(i),
            theme,
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

/// Most rows the terminal-leader popup enumerates before collapsing the
/// tail into a "+N more" row. Keeps the popup from swallowing the screen
/// when the agent roster is long. Sized so the fixed command menu (8
/// rows: snippets/focus/exit/jump + the tile chords, #286) never eats
/// the whole budget — a few agent-jump rows always show before the
/// overflow marker. `pub(crate)` so the model can bound `]]`-leader
/// highlight navigation to the rows the popup actually shows (#343).
pub(crate) const LEADER_MAX_ROWS: usize = 12;

/// Render the which-key popup for the armed terminal `]]` leader
/// (issues #205, #252). Lists the leader's command menu — the caller
/// orders the fixed commands (`]]s` snippets, `]]f` focus, `]]q` exit,
/// `` ]]` `` jump, the tile chords `]]|`/`]]-`/`]]<arrow>`/`]]x`) first,
/// then the agent-jump roster (`]]<digit>` → workspace) — plus a hint
/// that Esc cancels. The list is capped at
/// `LEADER_MAX_ROWS` with the tail collapsed to "+N more", so the head
/// (commands) always survives. Visual twin of [`render`], but the
/// binding set is the leader menu, so it takes the rows directly.
///
/// `highlight` is the row `j` / `k` have moved the selection to (`None`
/// in the direct-key default), drawn with a `fill`-backed caret so
/// `Enter` has a visible target (#343).
pub fn render_terminal_leader(
    frame: &mut Frame,
    area: Rect,
    escape_char: char,
    rows: &[(String, String)],
    highlight: Option<usize>,
) {
    let theme = crate::theme::current();
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

    let title = Line::from(Span::styled(
        format!(" {escape_char}{escape_char} "),
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

    for (i, (k, desc)) in rows.iter().take(shown).enumerate() {
        let y = panel.y + 2 + i as u16;
        if y >= panel.y + panel.height {
            break;
        }
        render_menu_row(
            frame,
            Rect {
                x: panel.x,
                y,
                width: panel.width,
                height: 1,
            },
            k,
            desc,
            highlight == Some(i),
            theme,
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
    // Footer hint on the bottom row of the panel. The commands (incl.
    // `q` exit) are listed above; Esc cancels back to the terminal. The
    // leader is non-timed (#252), so nothing leaves on its own.
    let hint_y = panel.y + panel.height.saturating_sub(1);
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            " Esc cancel ".to_string(),
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

    fn render_leader(rows: &[(String, String)]) -> String {
        render_leader_highlight(rows, None)
    }

    fn render_leader_highlight(rows: &[(String, String)], highlight: Option<usize>) -> String {
        let (w, h) = (80u16, 24u16);
        let mut term = Terminal::new(TestBackend::new(w, h)).unwrap();
        term.draw(|f| render_terminal_leader(f, Rect::new(0, 0, w, h), ']', rows, highlight))
            .unwrap();
        let buf = term.backend().buffer().clone();
        (0..h)
            .map(|y| (0..w).map(|x| buf[(x, y)].symbol()).collect::<String>())
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// The highlighted row carries a `▸` caret so `Enter` has a visible
    /// target; the others don't (#343).
    #[test]
    fn highlighted_row_shows_a_caret() {
        let rows = vec![
            ("s".to_string(), "snippets".to_string()),
            ("f".to_string(), "focus mode".to_string()),
            ("q".to_string(), "exit to sidebar".to_string()),
        ];
        let out = render_leader_highlight(&rows, Some(1));
        let caret_line = out
            .lines()
            .find(|l| l.contains('▸'))
            .expect("a row is caret-marked");
        assert!(
            caret_line.contains("focus mode"),
            "the caret sits on the highlighted row: {caret_line}"
        );
        assert_eq!(out.matches('▸').count(), 1, "only one row is highlighted");
        assert!(
            !render_leader(&rows).contains('▸'),
            "no caret without a highlight"
        );
    }

    /// The leader popup lists the command menu and an Esc hint.
    #[test]
    fn terminal_leader_shows_command_menu() {
        let rows = vec![
            ("s".to_string(), "snippets".to_string()),
            ("f".to_string(), "focus mode".to_string()),
            ("q".to_string(), "exit to sidebar".to_string()),
        ];
        let out = render_leader(&rows);
        assert!(out.contains("snippets"), "missing snippets row: {out}");
        assert!(out.contains("exit to sidebar"), "missing exit row: {out}");
        assert!(out.contains("Esc cancel"), "missing cancel hint: {out}");
    }

    /// With more rows than the popup caps at, the *head* of the list
    /// survives and the tail collapses to "+N more". The caller orders
    /// the fixed commands first precisely so `s`/`f`/`q` can never be the
    /// rows that get truncated (#252) — a user with a long agent roster
    /// must still see the exit.
    #[test]
    fn terminal_leader_truncates_the_tail_not_the_head() {
        let mut rows = vec![
            ("s".to_string(), "snippets".to_string()),
            ("f".to_string(), "focus mode".to_string()),
            ("q".to_string(), "exit to sidebar".to_string()),
            ("`".to_string(), "jump to workspace".to_string()),
        ];
        for i in 1..=9 {
            rows.push((i.to_string(), format!("agent-{i}")));
        }
        let out = render_leader(&rows);
        assert!(out.contains("snippets"), "head command dropped: {out}");
        assert!(
            out.contains("exit to sidebar"),
            "exit command dropped: {out}"
        );
        assert!(out.contains("more"), "overflow marker missing: {out}");
    }
}
