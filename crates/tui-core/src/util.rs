//! Small text utilities reused across the TUI: visual-width math
//! and ellipsis truncation. Lives at the crate root so the
//! `components::table` renderer can call it without taking a dep
//! on `components::sidebar` (where these helpers originally lived).
//!
//! Lazybox's text content is overwhelmingly ASCII + a handful of
//! box-drawing / Unicode marker chars (`▸`, `●`, `✓`, `❯`, `│`,
//! `▾`). Each of those is 1 cell in a monospaced terminal, so
//! `chars().count()` matches `unicode-width` in 99% of cases — and
//! we already use it that way. Centralising means there's one
//! place to swap in a real `unicode-width` dep if we ever take CJK
//! / emoji rendering seriously.

/// Visual width of a string in terminal cells.
pub fn visual_width(s: &str) -> usize {
    s.chars().count()
}

/// Visual width of a single character in terminal cells.
pub fn char_visual_width(_ch: char) -> usize {
    1
}

/// Truncate `s` so it fits in `budget` cells, adding `…` when
/// clipped. Returns `s` unchanged when it already fits.
pub fn truncate_ellipsis(s: &str, budget: usize) -> String {
    let w = visual_width(s);
    if w <= budget {
        return s.to_string();
    }
    if budget == 0 {
        return String::new();
    }
    let mut out: String = s.chars().take(budget.saturating_sub(1)).collect();
    out.push('…');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ascii_width_matches_byte_count() {
        assert_eq!(visual_width("hello"), 5);
    }

    #[test]
    fn box_drawing_chars_are_one_cell() {
        assert_eq!(visual_width("▸ "), 2);
        assert_eq!(visual_width("● "), 2);
        assert_eq!(visual_width("│"), 1);
    }

    #[test]
    fn truncate_fits_returns_input_unchanged() {
        assert_eq!(truncate_ellipsis("hello", 10), "hello");
    }

    #[test]
    fn truncate_clips_with_ellipsis() {
        assert_eq!(truncate_ellipsis("hello world", 5), "hell…");
    }

    #[test]
    fn truncate_budget_zero_empty() {
        assert_eq!(truncate_ellipsis("hello", 0), "");
    }
}
