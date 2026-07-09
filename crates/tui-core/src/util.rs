//! Small text utilities reused across the TUI: visual-width math
//! and ellipsis truncation. Lives at the crate root so the
//! `components::table` renderer can call it without taking a dep
//! on `components::sidebar` (where these helpers originally lived).
//!
//! Widths are real terminal cells (`unicode-width`), matching how
//! ratatui measures spans — a truncation budgeted in `chars()` lets
//! CJK/emoji text (issue titles are arbitrary user content) render
//! at up to twice the intended cells and blow the layout the budget
//! was protecting.

use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

/// Visual width of a string in terminal cells.
pub fn visual_width(s: &str) -> usize {
    UnicodeWidthStr::width(s)
}

/// Visual width of a single character in terminal cells.
pub fn char_visual_width(ch: char) -> usize {
    UnicodeWidthChar::width(ch).unwrap_or(0)
}

/// Truncate `s` so it fits in `budget` cells, adding `…` when
/// clipped. Returns `s` unchanged when it already fits.
pub fn truncate_ellipsis(s: &str, budget: usize) -> String {
    if visual_width(s) <= budget {
        return s.to_string();
    }
    if budget == 0 {
        return String::new();
    }
    let mut out = String::new();
    let mut used = 0usize;
    for ch in s.chars() {
        let w = char_visual_width(ch);
        if used + w > budget - 1 {
            break;
        }
        used += w;
        out.push(ch);
    }
    out.push('…');
    out
}

/// Truncate `s` to `budget` cells keeping both ends, with `…`
/// marking the cut in the middle. Use when the tail carries meaning
/// — e.g. a fixed actionable suffix ("… — press ! to jump") after a
/// variable-length name, where end-truncation would delete exactly
/// the part the message exists to deliver.
pub fn truncate_ellipsis_middle(s: &str, budget: usize) -> String {
    if visual_width(s) <= budget {
        return s.to_string();
    }
    if budget == 0 {
        return String::new();
    }
    if budget == 1 {
        return "…".to_string();
    }
    let keep = budget - 1;
    let head_budget = keep / 2;
    let tail_budget = keep - head_budget;

    let mut head = String::new();
    let mut used = 0usize;
    for ch in s.chars() {
        let w = char_visual_width(ch);
        if used + w > head_budget {
            break;
        }
        used += w;
        head.push(ch);
    }

    let mut tail_rev: Vec<char> = Vec::new();
    used = 0;
    for ch in s.chars().rev() {
        let w = char_visual_width(ch);
        if used + w > tail_budget {
            break;
        }
        used += w;
        tail_rev.push(ch);
    }

    head.push('…');
    head.extend(tail_rev.into_iter().rev());
    head
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
    fn wide_chars_are_two_cells() {
        assert_eq!(visual_width("好"), 2);
        assert_eq!(visual_width("日本語"), 6);
        assert_eq!(char_visual_width('好'), 2);
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

    /// The budget is cells, not chars: wide chars fill it twice as
    /// fast, and a char that would straddle the boundary is dropped
    /// rather than overflowing.
    #[test]
    fn truncate_budgets_wide_chars_in_cells() {
        assert_eq!(truncate_ellipsis("好好好好", 5), "好好…");
        assert_eq!(visual_width(&truncate_ellipsis("好好好好", 6)), 5);
        assert_eq!(truncate_ellipsis("好好好好", 6), "好好…");
    }

    #[test]
    fn middle_truncate_fits_returns_input_unchanged() {
        assert_eq!(truncate_ellipsis_middle("hello", 5), "hello");
    }

    #[test]
    fn middle_truncate_keeps_both_ends() {
        assert_eq!(
            truncate_ellipsis_middle("a long name — press ! to jump", 20),
            "a long na… ! to jump",
        );
    }

    #[test]
    fn middle_truncate_result_fits_budget() {
        let out = truncate_ellipsis_middle("a long name — press ! to jump", 20);
        assert!(visual_width(&out) <= 20);
    }

    #[test]
    fn middle_truncate_tiny_budgets() {
        assert_eq!(truncate_ellipsis_middle("hello world", 0), "");
        assert_eq!(truncate_ellipsis_middle("hello world", 1), "…");
        assert_eq!(truncate_ellipsis_middle("hello world", 2), "…d");
    }

    #[test]
    fn middle_truncate_wide_chars_stay_within_cells() {
        let out = truncate_ellipsis_middle(&"好".repeat(30), 15);
        assert!(visual_width(&out) <= 15, "{out:?}");
        assert!(out.contains('…'));
    }
}
