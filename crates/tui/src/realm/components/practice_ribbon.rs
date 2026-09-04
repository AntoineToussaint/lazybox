//! `practice_ribbon` — the unmistakable top strip shown while lazybox is
//! running the isolated practice world (`lazybox practice`, #1458).
//!
//! Onboarding's practice mode drives the *real* UI from a synthetic,
//! in-memory inbox so a new user can rehearse triage, watch an agent work,
//! and learn the keyboard without credentials or risk. The one hard rule is
//! that the simulator must be impossible to confuse with the real inbox —
//! so this bar sits above every pane in a loud color, states plainly that
//! nothing here is real, and always names the key that leaves. It is the
//! visible half of the isolation guarantee; the enforcing half is that the
//! practice flag can only be set with a `PracticeIsolation` proof (the
//! home has been redirected to a throwaway dir, and the store is in-memory),
//! so this bar can't be shown over a session that writes real state.
//!
//! Pure render — the quit key is passed in from `Model::view`.

use tuirealm::ratatui::Frame;
use tuirealm::ratatui::layout::Rect;
use tuirealm::ratatui::prelude::*;
use tuirealm::ratatui::widgets::Paragraph;

/// The full ribbon caption, given the effective quit chord (e.g. `q q`).
/// Split out so it can be asserted without a render harness.
pub fn label(quit_keys: &str) -> String {
    format!("🎓 PRACTICE — a safe sandbox to learn in, not your real inbox · {quit_keys} to leave")
}

/// Render the practice ribbon into `area` (a single full-width row).
/// `quit_keys` is the effective, post-override quit chord so the way out
/// is always on screen — the lesson of #100, where a user killed their
/// terminal because they could not find quit.
pub fn render(frame: &mut Frame, area: Rect, quit_keys: &str) {
    if area.height == 0 || area.width == 0 {
        return;
    }
    let theme = crate::theme::current();
    // A loud, filled bar: warn-colored ground with black text (caution-tape
    // contrast) so it reads as an alert, not a header. Filling first paints
    // the whole width even when the caption is shorter than the row.
    let ground = Style::default().bg(theme.warn).fg(Color::Black);
    frame.render_widget(Paragraph::new(Line::raw("")).style(ground), area);

    let full = label(quit_keys);
    // Clip to the row so a narrow terminal can't overflow the bar.
    let width = area.width as usize;
    let text: String = if full.chars().count() < width {
        format!(" {full}")
    } else {
        let budget = width.saturating_sub(2);
        let clipped: String = full.chars().take(budget).collect();
        format!(" {clipped}")
    };
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            text,
            ground.add_modifier(Modifier::BOLD),
        )))
        .style(ground),
        area,
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use tuirealm::ratatui::Terminal;
    use tuirealm::ratatui::backend::TestBackend;

    #[test]
    fn label_names_the_practice_world_and_the_way_out() {
        let text = label("q q");
        assert!(text.contains("PRACTICE"), "ribbon must announce practice");
        assert!(
            text.contains("not your real inbox"),
            "ribbon must disclaim real state"
        );
        assert!(
            text.contains("q q to leave"),
            "ribbon must name the quit key"
        );
    }

    fn render_to_string(w: u16, quit_keys: &str) -> String {
        let backend = TestBackend::new(w, 1);
        let mut term = Terminal::new(backend).unwrap();
        term.draw(|f| render(f, Rect::new(0, 0, w, 1), quit_keys))
            .unwrap();
        let buf = term.backend().buffer().clone();
        (0..w).map(|x| buf[(x, 0)].symbol()).collect::<String>()
    }

    #[test]
    fn render_paints_the_caption_on_a_wide_row() {
        let out = render_to_string(90, "q q");
        assert!(out.contains("PRACTICE"), "{out}");
        assert!(out.contains("q q to leave"), "{out}");
    }

    #[test]
    fn render_clips_without_panicking_on_a_narrow_row() {
        // Must not overflow a tight terminal — just render a prefix.
        let out = render_to_string(20, "q q");
        assert_eq!(out.chars().count(), 20, "row must fill exactly its width");
        assert!(out.contains("PRACTICE"), "{out}");
    }
}
