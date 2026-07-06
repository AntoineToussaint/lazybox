//! `focus_header` — the slim event strip shown at the top of focus
//! mode (issue #156).
//!
//! Focus mode hides the sidebar and activity pane to give the coding
//! agent's terminal the whole window. This one-row strip is the only
//! chrome that survives: it names the workspace the terminal belongs
//! to and carries a live tally of incoming work — unread activity,
//! agents waiting on input, failing CI, pending reviews — so a
//! heads-down user keeps situational awareness. Actionable signals
//! (asking / CI) render in their alert colors; quiet ones stay dim.
//!
//! Pure render — every value is passed in from `Model::view`.

use crate::components::sidebar::AttentionSummary;
use tuirealm::ratatui::Frame;
use tuirealm::ratatui::layout::Rect;
use tuirealm::ratatui::prelude::*;
use tuirealm::ratatui::widgets::Paragraph;

/// Render the focus-mode header into `area` (a single row).
/// `title` names the workspace whose terminal is showing; `hint` is
/// the short keybinding reminder pinned to the right edge.
pub fn render(frame: &mut Frame, area: Rect, title: &str, summary: AttentionSummary, hint: &str) {
    if area.height == 0 || area.width == 0 {
        return;
    }
    let theme = crate::theme::current();
    let bg = Style::default().bg(theme.surface);

    // Background fill so the strip reads as chrome distinct from the
    // terminal body below it.
    frame.render_widget(Paragraph::new(Line::raw("")).style(bg), area);

    // ── Left: focus glyph + workspace title + attention counts ──────
    let mut left: Vec<Span> = vec![
        Span::styled(
            " ◆ ",
            Style::default()
                .bg(theme.surface)
                .fg(theme.accent)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!("{title}  "),
            Style::default()
                .bg(theme.surface)
                .fg(theme.text_strong)
                .add_modifier(Modifier::BOLD),
        ),
    ];
    for (count, glyph, label, color, bold) in [
        (summary.asking, "!", "asking", theme.warn, true),
        (summary.ci_failing, "✗", "CI", theme.error, true),
        (summary.unread, "●", "new", theme.accent, false),
        (summary.review_pending, "⟳", "review", theme.warn, false),
    ] {
        if count == 0 {
            continue;
        }
        let mut style = Style::default().bg(theme.surface).fg(color);
        if bold {
            style = style.add_modifier(Modifier::BOLD);
        }
        left.push(Span::styled(format!("{glyph} {count} {label}  "), style));
    }
    frame.render_widget(Paragraph::new(Line::from(left)).style(bg), area);

    // ── Right: keybinding hint, right-aligned ───────────────────────
    let hint_w = (hint.chars().count() as u16).saturating_add(1);
    if hint_w < area.width {
        let hint_rect = Rect {
            x: area.x + area.width - hint_w,
            y: area.y,
            width: hint_w,
            height: 1,
        };
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                format!("{hint} "),
                Style::default().bg(theme.surface).fg(theme.text_dim),
            )))
            .style(bg),
            hint_rect,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tuirealm::ratatui::Terminal;
    use tuirealm::ratatui::backend::TestBackend;

    fn render_to_string(title: &str, summary: AttentionSummary, hint: &str) -> String {
        let mut term = Terminal::new(TestBackend::new(70, 1)).unwrap();
        term.draw(|f| render(f, Rect::new(0, 0, 70, 1), title, summary, hint))
            .unwrap();
        let buf = term.backend().buffer().clone();
        (0..buf.area.width)
            .map(|x| buf[(x, 0)].symbol())
            .collect::<String>()
    }

    #[test]
    fn shows_title_and_nonzero_counts() {
        let summary = AttentionSummary {
            unread: 3,
            asking: 1,
            ci_failing: 0,
            review_pending: 2,
        };
        let line = render_to_string("fix-the-thing", summary, "]]] exit");
        assert!(line.contains("fix-the-thing"), "title missing: {line:?}");
        assert!(line.contains("1 asking"), "asking count missing: {line:?}");
        assert!(line.contains("3 new"), "unread count missing: {line:?}");
        assert!(line.contains("2 review"), "review count missing: {line:?}");
        // ci_failing is zero, so its segment is omitted entirely.
        assert!(!line.contains("CI"), "zero CI should not render: {line:?}");
        assert!(line.contains("]]] exit"), "hint missing: {line:?}");
    }

    #[test]
    fn quiet_workspace_shows_only_title_and_hint() {
        let line = render_to_string("scratch", AttentionSummary::default(), "]]] exit");
        assert!(line.contains("scratch"));
        assert!(line.contains("]]] exit"));
        assert!(!line.contains("new"));
        assert!(!line.contains("asking"));
    }
}
