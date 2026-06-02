//! Minimal vertical scroll-position indicator. A thin track with a
//! proportional thumb, drawn in a single column on the right edge of a
//! scrollable view. Auto-hides when everything fits, so callers can
//! hand it the gutter column unconditionally and only see it when
//! there's actually content off-screen.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

const TRACK: &str = "│";
const THUMB: &str = "█";

/// Draw a vertical scrollbar in `track` (a 1-cell-wide column).
///
/// * `total` — total number of content rows.
/// * `viewport` — number of rows currently visible.
/// * `offset` — index of the topmost visible row (0 == top).
///
/// No-op when the content fits (`total <= viewport`) or there's no
/// room to draw — that's the auto-hide the indicator is supposed to
/// have.
pub fn render_vertical(
    frame: &mut Frame,
    track: Rect,
    total: usize,
    viewport: usize,
    offset: usize,
) {
    if track.width == 0 || track.height == 0 || viewport == 0 || total <= viewport {
        return;
    }
    let theme = crate::theme::current();
    let track_h = track.height as usize;

    // Thumb height tracks the visible fraction, floored at one cell so
    // it never disappears on very long content.
    let thumb_h = ((track_h * viewport) / total).clamp(1, track_h);

    // Spread the offset across the free travel of the track. Rounded so
    // the thumb sits flush at the bottom exactly when scrolled to the
    // end, and flush at the top when at the start.
    let max_offset = total - viewport;
    let travel = track_h - thumb_h;
    let thumb_top = (travel * offset.min(max_offset) + max_offset / 2)
        .checked_div(max_offset)
        .unwrap_or(0);

    let track_style = theme.divider();
    let thumb_style = Style::default().fg(theme.accent);
    let lines: Vec<Line> = (0..track_h)
        .map(|row| {
            if row >= thumb_top && row < thumb_top + thumb_h {
                Line::from(Span::styled(THUMB, thumb_style))
            } else {
                Line::from(Span::styled(TRACK, track_style))
            }
        })
        .collect();
    frame.render_widget(Paragraph::new(lines), track);
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    fn render(total: usize, viewport: usize, offset: usize, height: u16) -> String {
        let backend = TestBackend::new(1, height);
        let mut term = Terminal::new(backend).unwrap();
        term.draw(|frame| {
            render_vertical(frame, Rect::new(0, 0, 1, height), total, viewport, offset);
        })
        .unwrap();
        let buffer = term.backend().buffer();
        (0..height)
            .map(|y| buffer[(0, y)].symbol().to_string())
            .collect()
    }

    #[test]
    fn hides_when_content_fits() {
        // Empty cells (rendered as spaces) → nothing drawn.
        assert_eq!(render(5, 10, 0, 8), "        ");
        assert_eq!(render(8, 8, 0, 8), "        ");
    }

    #[test]
    fn thumb_at_top_when_offset_zero() {
        let bar = render(40, 10, 0, 8);
        assert!(bar.starts_with('█'), "expected thumb at top, got {bar:?}");
        assert!(bar.ends_with('│'), "expected track at bottom, got {bar:?}");
    }

    #[test]
    fn thumb_at_bottom_when_fully_scrolled() {
        let bar = render(40, 10, 30, 8);
        assert!(bar.ends_with('█'), "expected thumb at bottom, got {bar:?}");
        assert!(bar.starts_with('│'), "expected track at top, got {bar:?}");
    }

    #[test]
    fn thumb_never_smaller_than_one_cell() {
        // Huge content, tiny viewport: thumb still occupies a cell.
        let bar = render(10_000, 1, 0, 8);
        assert_eq!(bar.matches('█').count(), 1);
    }
}
