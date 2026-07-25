//! `ScrollableModal` — the shared core behind the read-only scrolling
//! reader modals (`sync_status`, `messages`, `snippet_browser`,
//! `markdown_modal`).
//!
//! Those four each re-implemented the same scroll-offset key protocol
//! (Down/`j`, Up/`k`, PageDown, PageUp, Ctrl-d, Ctrl-u, Home/`g`), the
//! same bottom clamp, and the same centered rounded-frame chrome (issue
//! #549). This module owns that once. Each reader keeps its own `scroll`
//! / `body_height` fields (so the existing in-module tests keep reaching
//! `comp.scroll` directly) and its own body-line rendering, dismiss
//! policy, and extra keys (`messages` clears with `c`, `snippet_browser`
//! edits the YAML with `e`, `markdown_modal` adds End/`G` and stays inert
//! on unknown keys instead of dismissing):
//!
//! - [`handle_scroll_key`] applies the common scroll keys, returning
//!   whether the key was a scroll command — a reader calls it first and
//!   only reaches its own keys when it returns `false`.
//! - [`max_scroll`] is the shared bottom clamp.
//! - [`centered_rect`] and [`draw_frame`] are the shared chrome.

use crate::theme::Theme;
use tuirealm::event::{Key, KeyEvent, KeyModifiers};
use tuirealm::ratatui::Frame;
use tuirealm::ratatui::layout::Rect;
use tuirealm::ratatui::widgets::{Block, BorderType, Borders, Clear};

/// Apply the shared scroll-key protocol to `scroll`, paging by
/// `body_height`. Returns `true` when the key was a scroll command (the
/// caller then treats it as consumed); `false` leaves `scroll` untouched
/// so the caller can handle its own keys. Clamp-to-bottom is deferred to
/// the next render via [`max_scroll`] — a reader can't know its total
/// line count here without re-deriving the body.
pub fn handle_scroll_key(scroll: &mut u16, body_height: u16, key: &KeyEvent) -> bool {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    let page = body_height.max(1);
    match key.code {
        Key::Down | Key::Char('j') => *scroll = scroll.saturating_add(1),
        Key::Up | Key::Char('k') => *scroll = scroll.saturating_sub(1),
        Key::PageDown => *scroll = scroll.saturating_add(page),
        Key::PageUp => *scroll = scroll.saturating_sub(page),
        Key::Char('d') if ctrl => *scroll = scroll.saturating_add((page / 2).max(1)),
        Key::Char('u') if ctrl => *scroll = scroll.saturating_sub((page / 2).max(1)),
        Key::Home | Key::Char('g') => *scroll = 0,
        _ => return false,
    }
    true
}

/// The maximum scroll offset that keeps the last line on screen — so a
/// short body (or an `End`-jump to `u16::MAX`) can't leave blank rows
/// scrolled off the top.
pub fn max_scroll(total_lines: usize, body_height: u16) -> u16 {
    (total_lines as u16).saturating_sub(body_height.max(1))
}

/// Center a modal of size `w × h` within `area`.
pub fn centered_rect(area: Rect, w: u16, h: u16) -> Rect {
    let x = area.x + area.width.saturating_sub(w) / 2;
    let y = area.y + area.height.saturating_sub(h) / 2;
    Rect::new(x, y, w, h)
}

/// Draw the shared rounded modal frame — a `Clear` plus a bordered block
/// with `title` — and return the inner content rect. The caller reserves
/// its own hint row / gutter inside that rect and decides what to do when
/// it's too small.
pub fn draw_frame(frame: &mut Frame, modal: Rect, title: &str, theme: &Theme) -> Rect {
    frame.render_widget(Clear, modal);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(theme.modal_border())
        .title(title.to_string())
        .title_style(theme.modal_title());
    let inner = block.inner(modal);
    frame.render_widget(block, modal);
    inner
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ke(code: Key) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }
    fn ctrl(c: char) -> KeyEvent {
        KeyEvent::new(Key::Char(c), KeyModifiers::CONTROL)
    }

    #[test]
    fn line_and_page_scroll_both_directions() {
        let mut s = 0u16;
        assert!(handle_scroll_key(&mut s, 10, &ke(Key::Down)));
        assert_eq!(s, 1);
        assert!(handle_scroll_key(&mut s, 10, &ke(Key::Char('j'))));
        assert_eq!(s, 2);
        assert!(handle_scroll_key(&mut s, 10, &ke(Key::PageDown)));
        assert_eq!(s, 12);
        assert!(handle_scroll_key(&mut s, 10, &ke(Key::PageUp)));
        assert_eq!(s, 2);
        assert!(handle_scroll_key(&mut s, 10, &ke(Key::Up)));
        assert_eq!(s, 1);
        assert!(handle_scroll_key(&mut s, 10, &ke(Key::Char('k'))));
        assert_eq!(s, 0);
    }

    #[test]
    fn half_page_uses_ctrl_d_and_u_and_never_stalls() {
        let mut s = 0u16;
        // page/2 for a 10-row viewport is 5.
        assert!(handle_scroll_key(&mut s, 10, &ctrl('d')));
        assert_eq!(s, 5);
        assert!(handle_scroll_key(&mut s, 10, &ctrl('u')));
        assert_eq!(s, 0);
        // A 1-row viewport still advances by at least 1 (no stall).
        assert!(handle_scroll_key(&mut s, 1, &ctrl('d')));
        assert_eq!(s, 1);
    }

    #[test]
    fn home_and_g_jump_to_top() {
        let mut s = 42u16;
        assert!(handle_scroll_key(&mut s, 10, &ke(Key::Home)));
        assert_eq!(s, 0);
        s = 42;
        assert!(handle_scroll_key(&mut s, 10, &ke(Key::Char('g'))));
        assert_eq!(s, 0);
    }

    #[test]
    fn saturating_at_zero() {
        let mut s = 0u16;
        assert!(handle_scroll_key(&mut s, 10, &ke(Key::Up)));
        assert_eq!(s, 0, "up at the top stays at 0");
    }

    #[test]
    fn non_scroll_keys_are_left_for_the_caller() {
        let mut s = 7u16;
        for code in [
            Key::Esc,
            Key::Enter,
            Key::Char('e'),
            Key::Char('c'),
            Key::Char('q'),
        ] {
            assert!(!handle_scroll_key(&mut s, 10, &ke(code)));
        }
        // `d`/`u` without Ctrl are not scroll commands either.
        assert!(!handle_scroll_key(&mut s, 10, &ke(Key::Char('d'))));
        assert!(!handle_scroll_key(&mut s, 10, &ke(Key::Char('u'))));
        assert_eq!(s, 7, "scroll untouched by non-scroll keys");
    }

    #[test]
    fn max_scroll_clamps_short_and_tall_bodies() {
        // Body fits the viewport → no scroll room.
        assert_eq!(max_scroll(5, 10), 0);
        // 40 lines in a 10-row viewport → 30 rows of scroll.
        assert_eq!(max_scroll(40, 10), 30);
        // A zero viewport is treated as one row.
        assert_eq!(max_scroll(3, 0), 2);
    }

    #[test]
    fn centered_rect_centers_within_area() {
        let area = Rect::new(0, 0, 100, 40);
        let r = centered_rect(area, 80, 24);
        assert_eq!((r.x, r.y, r.width, r.height), (10, 8, 80, 24));
    }
}
