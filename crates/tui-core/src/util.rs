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
//!
//! The truncation helpers return `Cow` so the fits case (the
//! overwhelming majority) doesn't allocate — they run inside
//! per-frame render paths.

use std::borrow::Cow;
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

/// Visual width of a string in terminal cells.
pub fn visual_width(s: &str) -> usize {
    UnicodeWidthStr::width(s)
}

/// Visual width of a single character in terminal cells.
pub fn char_visual_width(ch: char) -> usize {
    UnicodeWidthChar::width(ch).unwrap_or(0)
}

/// Extended grapheme clusters in display order.
pub fn graphemes(s: &str) -> impl DoubleEndedIterator<Item = &str> {
    UnicodeSegmentation::graphemes(s, true)
}

/// Truncate `s` so it fits in `budget` cells, adding `…` when
/// clipped. Returns `s` unchanged when it already fits.
pub fn truncate_ellipsis(s: &str, budget: usize) -> Cow<'_, str> {
    if visual_width(s) <= budget {
        return Cow::Borrowed(s);
    }
    if budget == 0 {
        return Cow::Borrowed("");
    }
    let mut out = String::new();
    let mut used = 0usize;
    for grapheme in graphemes(s) {
        let w = visual_width(grapheme);
        if used + w > budget - 1 {
            break;
        }
        used += w;
        out.push_str(grapheme);
    }
    // Never render a double ellipsis: text that was already
    // ellipsis-truncated upstream (e.g. a notice slug) can land its
    // own `…` exactly at the cut point.
    if !out.ends_with('…') {
        out.push('…');
    }
    Cow::Owned(out)
}

/// Truncate `s` to `budget` cells keeping both ends, with `…`
/// marking the cut in the middle. Use when the tail carries meaning
/// — e.g. a fixed actionable suffix ("… — press ! to jump") after a
/// variable-length name, where end-truncation would delete exactly
/// the part the message exists to deliver.
pub fn truncate_ellipsis_middle(s: &str, budget: usize) -> Cow<'_, str> {
    if visual_width(s) <= budget {
        return Cow::Borrowed(s);
    }
    if budget == 0 {
        return Cow::Borrowed("");
    }
    if budget == 1 {
        return Cow::Borrowed("…");
    }
    let keep = budget - 1;
    let head_budget = keep / 2;
    let tail_budget = keep - head_budget;

    let mut head = String::new();
    let mut used = 0usize;
    for grapheme in graphemes(s) {
        let w = visual_width(grapheme);
        if used + w > head_budget {
            break;
        }
        used += w;
        head.push_str(grapheme);
    }

    let mut tail_rev: Vec<&str> = Vec::new();
    used = 0;
    for grapheme in graphemes(s).rev() {
        let w = visual_width(grapheme);
        if used + w > tail_budget {
            break;
        }
        used += w;
        tail_rev.push(grapheme);
    }

    // As in `truncate_ellipsis`: don't double up with an ellipsis
    // the input already carries at the cut point.
    if !head.ends_with('…') && tail_rev.last() != Some(&"…") {
        head.push('…');
    }
    for grapheme in tail_rev.into_iter().rev() {
        head.push_str(grapheme);
    }
    Cow::Owned(head)
}

/// Short workspace identifier for one-line notices. Issue/PR
/// workspace names are full issue titles; interpolating one raw
/// displaces the rest of the message (#291). Middle truncation
/// keeps the title's head *and* tail — related issues often share a
/// long prefix, so the tail is what disambiguates, and in focus mode
/// (no sidebar) the notice can be the only surface naming the
/// workspace.
pub fn notice_slug(name: &str) -> Cow<'_, str> {
    truncate_ellipsis_middle(name, 24)
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
        assert!(matches!(
            truncate_ellipsis("hello", 10),
            Cow::Borrowed("hello")
        ));
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
    fn truncate_preserves_emoji_grapheme_clusters() {
        assert_eq!(truncate_ellipsis("👩‍💻abc", 4), "👩‍💻a…");
        assert_eq!(truncate_ellipsis("👩‍💻abc", 3), "👩‍💻…");
        assert_eq!(truncate_ellipsis_middle("ab👩‍💻cd", 5), "ab…cd");
    }

    /// Input already carrying an `…` at the cut point must not render
    /// `……` when re-truncated by a downstream layer.
    #[test]
    fn truncate_never_doubles_an_ellipsis() {
        assert_eq!(truncate_ellipsis("abcd…tail", 6), "abcd…");
        // Head side: keep=8 → head is exactly "abc…".
        assert_eq!(truncate_ellipsis_middle("abc…defghij", 9), "abc…ghij");
        // Tail side: keep=8 → tail is exactly "…hij".
        assert_eq!(truncate_ellipsis_middle("abcdefg…hij", 9), "abcd…hij");
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
        let wide = "好".repeat(30);
        let out = truncate_ellipsis_middle(&wide, 15);
        assert!(visual_width(&out) <= 15, "{out:?}");
        assert!(out.contains('…'));
    }

    #[test]
    fn notice_slug_passes_short_names_through() {
        assert_eq!(notice_slug("Ship it"), "Ship it");
    }

    /// Long prefixes are what related issue titles share; the slug
    /// keeps the tail so two siblings stay distinguishable.
    #[test]
    fn notice_slug_keeps_the_disambiguating_tail() {
        let a = "Footer notices hide the shortcut hints";
        let b = "Footer notices hide the sidebar badge";
        let (sa, sb) = (notice_slug(a), notice_slug(b));
        assert_ne!(sa, sb);
        assert!(sa.ends_with("hints"));
        assert!(visual_width(&sa) <= 24);
    }
}
