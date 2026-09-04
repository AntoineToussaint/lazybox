//! `practice_banner` — the permanent, unmistakable chrome that marks a
//! *practice* session (issue #1459).
//!
//! Practice mode is a full simulated inbox a new user can press every key
//! in with no consequence. A simulator that resembles the real inbox is a
//! trap, so this one-row banner is pinned to the very top of the window in
//! every screen (panes, focus mode, modals behind it) and never fades.
//!
//! It must read as "not real" even on a monochrome terminal, so it does not
//! rely on colour alone: the whole row is reverse-video (swapped fg/bg) and
//! bracketed by block glyphs, and the words say what it is and how to leave.

use tuirealm::ratatui::Frame;
use tuirealm::ratatui::layout::Rect;
use tuirealm::ratatui::prelude::*;
use tuirealm::ratatui::widgets::Paragraph;

/// Text pinned to the right edge — the always-visible exit affordance
/// criterion 4 asks for.
const EXIT_HINT: &str = "Ctrl-C to exit ";

/// Render the practice banner into `area` (a single row). The style is
/// reverse-video with a warning tint so it is legible with or without
/// colour; `paint_reversed` fills the whole width so no real-inbox
/// background bleeds through at the edges.
pub fn render(frame: &mut Frame, area: Rect) {
    if area.height == 0 || area.width == 0 {
        return;
    }
    // Reverse-video + bold is the one treatment that survives a monochrome
    // terminal: a terminal that renders no colour still honours REVERSED, so
    // the row is a solid bar edge to edge whether or not colour is available
    // — criterion 4's "legible without colour". No explicit fg/bg, precisely
    // so it does not depend on the palette.
    let base = Style::default().add_modifier(Modifier::BOLD | Modifier::REVERSED);

    // Fill the row first so the bar spans edge to edge under the text.
    frame.render_widget(Paragraph::new(Line::raw("")).style(base), area);

    let label = "▓ PRACTICE MODE — a simulated inbox · nothing here is real ";
    let mut spans: Vec<Span> = vec![Span::styled(label, base)];

    // Right-align the exit hint when the row is wide enough; otherwise the
    // label alone still carries the message.
    let used = label.chars().count() + EXIT_HINT.chars().count();
    if area.width as usize > used {
        let pad = area.width as usize - used;
        spans.push(Span::styled(" ".repeat(pad), base));
        spans.push(Span::styled(EXIT_HINT, base));
    }

    frame.render_widget(Paragraph::new(Line::from(spans)).style(base), area);
}

/// Carve the top row of `area` for the banner, returning `(banner, rest)`.
/// Mirrors `crate::realm::model::helpers::split_for_footer` so the banner
/// composes with the footer split without either knowing about the other.
pub fn split_for_banner(area: Rect) -> (Rect, Rect) {
    if area.height < 2 {
        return (Rect::default(), area);
    }
    let banner = Rect {
        x: area.x,
        y: area.y,
        width: area.width,
        height: 1,
    };
    let rest = Rect {
        x: area.x,
        y: area.y + 1,
        width: area.width,
        height: area.height - 1,
    };
    (banner, rest)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tuirealm::ratatui::Terminal;
    use tuirealm::ratatui::backend::TestBackend;

    fn render_row(width: u16) -> (String, tuirealm::ratatui::buffer::Buffer) {
        let mut term = Terminal::new(TestBackend::new(width, 1)).unwrap();
        term.draw(|f| render(f, Rect::new(0, 0, width, 1))).unwrap();
        let buf = term.backend().buffer().clone();
        let line = (0..buf.area.width)
            .map(|x| buf[(x, 0)].symbol())
            .collect::<String>();
        (line, buf)
    }

    #[test]
    fn banner_names_practice_and_the_exit() {
        let (line, _) = render_row(80);
        assert!(
            line.contains("PRACTICE MODE"),
            "banner missing label: {line:?}"
        );
        assert!(
            line.contains("Ctrl-C to exit"),
            "banner missing exit hint: {line:?}"
        );
    }

    #[test]
    fn banner_is_reverse_video_so_it_reads_without_colour() {
        // Criterion 4: legible on a monochrome terminal. The whole row must
        // carry REVERSED, which a no-colour terminal still honours as a solid
        // inverted bar — so it never depends on the palette.
        let (_, buf) = render_row(80);
        for x in 0..buf.area.width {
            assert!(
                buf[(x, 0)]
                    .style()
                    .add_modifier
                    .contains(Modifier::REVERSED),
                "cell {x} is not reverse-video — the bar would vanish without colour"
            );
        }
    }

    #[test]
    fn narrow_row_still_shows_the_label() {
        // Too narrow for the right-aligned exit hint, but the label survives.
        let (line, _) = render_row(30);
        assert!(
            line.contains("PRACTICE"),
            "narrow banner dropped the label: {line:?}"
        );
    }

    #[test]
    fn split_reserves_exactly_the_top_row() {
        let (banner, rest) = split_for_banner(Rect::new(0, 0, 40, 20));
        assert_eq!(banner, Rect::new(0, 0, 40, 1));
        assert_eq!(rest, Rect::new(0, 1, 40, 19));
    }

    #[test]
    fn split_degrades_when_there_is_no_room() {
        // One row: nothing to split — give it all to the content, not the
        // banner, so a tiny window still renders the app.
        let (banner, rest) = split_for_banner(Rect::new(0, 0, 40, 1));
        assert_eq!(banner, Rect::default());
        assert_eq!(rest, Rect::new(0, 0, 40, 1));
    }
}
